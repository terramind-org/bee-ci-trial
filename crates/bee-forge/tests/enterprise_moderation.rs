//! Moderation (PRODUCT §4.21): blocks, reports, interaction limits, comment minimizing,
//! locks, moderators, first-time contributors, flood defenses and takedowns, enforced at the
//! write path by `Moderation::check_write` and served through `ModerationService`.
//!
//! The scenarios are named after AS-MOD-01 (a moderator agent hides spam, blocks its author
//! and sets interaction limits). Store behaviour runs on memory, SQLite and, when
//! `BEE_TEST_PG` is set, Postgres.

use crate::common;
use crate::enterprise_support as support;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_forge::Cx;
use bee_forge::audit::{EntryKey, org_scope};
use bee_forge::enterprise::common::TeamDirectory;
use bee_forge::enterprise::moderation::{
    InMemoryComments, InMemoryStanding, Moderation, ModerationConfig, ModerationService, Outcome,
    WriteCheck, WriteKind,
};
use bee_forge::error::{ForgeError, Result};
use bee_forge::events::org_log;
use bee_forge::hooks::PrincipalDirectory;
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::module::Registry;
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_policy::Action;
use bee_proto::v1::moderation_service_server::ModerationService as Rpc;
use bee_proto::v1::*;
use common::{actions, reason, req};

const T0: u64 = 1_800_000_000_000;
const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

/// An in-memory team directory.
struct FakeTeams {
    teams: RwLock<BTreeMap<String, (String, Vec<String>)>>,
}

#[async_trait]
impl TeamDirectory for FakeTeams {
    async fn is_team_member(&self, _cx: &Cx, team_id: &str, principal_id: &str) -> Result<bool> {
        Ok(self
            .teams
            .read()
            .unwrap()
            .get(team_id)
            .is_some_and(|(_, m)| m.iter().any(|p| p == principal_id)))
    }

    async fn team_org(&self, _cx: &Cx, team_id: &str) -> Result<String> {
        self.teams
            .read()
            .unwrap()
            .get(team_id)
            .map(|(o, _)| o.clone())
            .ok_or_else(|| ForgeError::not_found(format!("team {team_id}")))
    }
}

struct W {
    env: common::Env,
    m: Arc<Moderation>,
    svc: ModerationService,
    standing: Arc<InMemoryStanding>,
    comments: Arc<InMemoryComments>,
    teams: Arc<FakeTeams>,
    clock: Arc<ManualClock>,
    org: Org,
    repo: Repo,
    other: Repo,
    private: Repo,
}

async fn world() -> W {
    let clock = Arc::new(ManualClock::new(T0));
    world_on(Arc::new(MemoryStore::new()), clock).await
}

async fn world_on(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>) -> W {
    let env = common::env_with(store, clock.clone() as Arc<dyn Clock>).await;
    for id in [
        "mod",
        "mod2",
        "maint",
        "spammer",
        "newcomer",
        "siteadmin",
        "agent-a",
        "agent-b",
    ] {
        env.directory.register(
            Principal {
                id: id.into(),
                kind: PrincipalKind::Human as i32,
                display: id.into(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![],
        );
    }
    let org = support::make_org(&env, "acme").await;
    let repo = support::make_repo_vis(&env, &org, "app", Visibility::Public).await;
    let other = support::make_repo_vis(&env, &org, "other", Visibility::Public).await;
    let private = support::make_repo(&env, &org, "secret").await;
    for (who, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("reader", Role::Member),
        ("maint", Role::Member),
        ("mod", Role::Moderator),
        ("agent", Role::Moderator),
    ] {
        support::add_member(&env, &org, who, role).await;
    }
    for (repo, who, role) in [
        (&repo, "writer", RepoRole::Write),
        (&other, "writer", RepoRole::Write),
        (&repo, "maint", RepoRole::Maintain),
    ] {
        let p = env.directory.get(who).await.unwrap();
        env.forge
            .orgs
            .add_collaborator(&support::cx("owner"), &repo.id, p, role, "")
            .await
            .unwrap();
    }
    let standing = Arc::new(InMemoryStanding::new());
    let comments = Arc::new(InMemoryComments::new());
    let teams = Arc::new(FakeTeams {
        teams: RwLock::new(BTreeMap::new()),
    });
    let cfg = ModerationConfig::new(standing.clone())
        .with_site_admins(["siteadmin"])
        .with_teams(teams.clone())
        .with_comments(comments.clone());
    let m = Moderation::new(env.forge.clone(), cfg);
    let svc = ModerationService::new(m.clone(), ForgeServices::new(env.forge.clone()));
    W {
        env,
        m,
        svc,
        standing,
        comments,
        teams,
        clock,
        org,
        repo,
        other,
        private,
    }
}

fn rr(full: &str) -> RepoRef {
    RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.into())),
    }
}

fn subj(kind: SubjectKind, id: &str) -> SubjectRef {
    SubjectRef {
        kind: kind as i32,
        repo: Some(rr("acme/app")),
        id: id.into(),
    }
}

fn org_block(name: &str) -> Option<BlockScope> {
    Some(BlockScope {
        scope: Some(block_scope::Scope::Org(name.into())),
    })
}

fn personal_block() -> Option<BlockScope> {
    Some(BlockScope {
        scope: Some(block_scope::Scope::Personal(true)),
    })
}

fn lim_repo(full: &str) -> Option<InteractionLimitScope> {
    Some(InteractionLimitScope {
        scope: Some(interaction_limit_scope::Scope::Repo(rr(full))),
    })
}

fn lim_org(name: &str) -> Option<InteractionLimitScope> {
    Some(InteractionLimitScope {
        scope: Some(interaction_limit_scope::Scope::Org(name.into())),
    })
}

fn agent_cx(id: &str, owner: &str) -> Cx {
    Cx::new(id, PrincipalKind::Agent).with_chain(vec![id.into(), owner.into()])
}

fn wc(who: &str, repo: &Repo, kind: WriteKind) -> WriteCheck {
    WriteCheck::new(&support::cx(who), &repo.id, &repo.org_id, kind)
}

fn wc_agent(id: &str, owner: &str, repo: &Repo, kind: WriteKind) -> WriteCheck {
    WriteCheck::new(&agent_cx(id, owner), &repo.id, &repo.org_id, kind)
}

impl W {
    fn tok(&self, id: &str) -> String {
        self.env.full(id)
    }

    fn agent_tok(&self) -> String {
        self.env
            .agent_token("owner", actions(&[Action::Read, Action::Triage]))
    }

    fn add_comment(&self, id: &str, author: &str, body: &str) {
        self.comments.insert(
            &self.org.id,
            "",
            Comment {
                id: id.into(),
                subject: Some(subj(SubjectKind::Issue, "1")),
                author: Some(Principal {
                    id: author.into(),
                    kind: PrincipalKind::Human as i32,
                    ..Default::default()
                }),
                body: body.into(),
                ..Default::default()
            },
        );
    }

    async fn audit(&self) -> Vec<AuditEntry> {
        self.env.forge.flush().await.unwrap();
        let (v, _) = self
            .env
            .forge
            .audit
            .query(
                &Cx::system(),
                &org_scope(&self.org.id),
                &AuditLogFilter {
                    action_prefixes: vec!["moderation.".into()],
                    ..Default::default()
                },
                true,
                "",
                500,
            )
            .await
            .unwrap();
        v
    }

    async fn event_kinds(&self) -> Vec<String> {
        self.env.forge.flush().await.unwrap();
        let (v, _) = self
            .env
            .forge
            .events
            .read(
                &Cx::system(),
                &org_log(&self.org.id),
                0,
                &["moderation.*".to_string()],
                1000,
            )
            .await
            .unwrap();
        v.into_iter().map(|e| e.kind).collect()
    }

    async fn set_flood(&self, f: SetFloodLimitsRequest) -> ModerationFloodLimits {
        self.svc
            .set_flood_limits(req(
                &self.tok("owner"),
                SetFloodLimitsRequest {
                    repo: Some(rr("acme/app")),
                    ..f
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn limit_repo(&self, level: InteractionLimitLevel, agents: bool) {
        self.svc
            .set_interaction_limit(req(
                &self.tok("owner"),
                SetInteractionLimitRequest {
                    scope: lim_repo("acme/app"),
                    level: level as i32,
                    duration_seconds: 86_400,
                    applies_to_agents: agents,
                },
            ))
            .await
            .unwrap();
    }
}

fn is_subcode(e: &ForgeError, reason: ErrorReason, sub: &str) -> bool {
    e.reason == reason && e.subcode == sub
}

// ---------------------------------------------------------------------------------------
// AS-MOD-01
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_mod_01_moderator_agent_hides_spam_blocks_author_sets_limits() {
    let w = world().await;
    w.add_comment("c-spam-1", "spammer", "buy cheap pills now");
    w.add_comment("c-spam-2", "spammer", "visit my store");
    w.add_comment("c-ok", "writer", "looks good to me");
    let agent = w.agent_tok();

    // Hide the spam comment: the body stays, the reason shows.
    let c = w
        .svc
        .minimize_comment(req(
            &agent,
            MinimizeCommentRequest {
                comment_id: "c-spam-1".into(),
                reason: MinimizedReason::Spam as i32,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.id, "c-spam-1");
    assert_eq!(c.minimized_reason, MinimizedReason::Spam as i32);
    assert_eq!(c.body, "buy cheap pills now");

    // Block the author in the org and hide everything else they wrote.
    let b = w
        .svc
        .block_user(req(
            &agent,
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "spammer".into(),
                reason: "spam".into(),
                hide_content: true,
                source: Some(subj(SubjectKind::Comment, "c-spam-1")),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(b.blocked.unwrap().id, "spammer");
    assert_eq!(b.blocked_by.unwrap().id, "agent");
    assert!(matches!(
        b.scope.unwrap().scope,
        Some(block_scope::Scope::Org(n)) if n == "acme"
    ));
    let hidden =
        w.m.comment_state(&support::cx("owner"), "c-spam-2")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(hidden.reason, MinimizedReason::Spam as i32);
    assert!(
        w.m.comment_state(&support::cx("owner"), "c-ok")
            .await
            .unwrap()
            .is_none(),
        "other authors' comments stay visible"
    );

    // Limit interactions on the repo to prior contributors.
    let l = w
        .svc
        .set_interaction_limit(req(
            &agent,
            SetInteractionLimitRequest {
                scope: lim_repo("acme/app"),
                level: InteractionLimitLevel::ContributorsOnly as i32,
                duration_seconds: 86_400,
                applies_to_agents: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.level, InteractionLimitLevel::ContributorsOnly as i32);
    assert_eq!(l.set_by.unwrap().id, "agent");

    // Enforcement at the write path.
    let e =
        w.m.check_write(&wc("spammer", &w.repo, WriteKind::Comment))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::Blocked, "{e:?}");
    let e =
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Issue))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InteractionLimited, "{e:?}");
    assert!(e.retry_after_ms > 0);
    assert!(
        w.m.check_write(&wc("writer", &w.repo, WriteKind::Comment))
            .await
            .unwrap()
            .allow,
        "collaborators are never limited"
    );
    // Another repo is untouched by the repo limit; the org block follows the author.
    assert!(
        w.m.check_write(&wc("newcomer", &w.other, WriteKind::Comment))
            .await
            .is_ok()
    );
    assert_eq!(
        w.m.check_write(&wc("spammer", &w.other, WriteKind::Reaction))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Blocked
    );

    // The audit chain holds an entry for each action, by the agent for its owner.
    let entries = w.audit().await;
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    for want in [
        "moderation.minimize",
        "moderation.block",
        "moderation.limit_set",
    ] {
        assert!(actions.contains(&want), "{want} missing in {actions:?}");
    }
    let block = entries
        .iter()
        .find(|e| e.action == "moderation.block")
        .unwrap();
    assert_eq!(block.actor.as_ref().unwrap().id, "agent");
    assert_eq!(block.on_behalf_of, "owner");
    assert_eq!(block.delegation_chain, vec!["agent", "owner"]);
    assert!(
        block.after.contains("c-spam-1"),
        "source recorded: {}",
        block.after
    );
    let v = w
        .env
        .forge
        .audit
        .verify(&Cx::system(), &org_scope(&w.org.id), 0, 0, &[])
        .await
        .unwrap();
    assert!(v.ok, "{v:?}");
    let kinds = w.event_kinds().await;
    for want in [
        "moderation.blocked",
        "moderation.limit_set",
        "moderation.comment_minimized",
    ] {
        assert!(
            kinds.iter().any(|k| k == want),
            "{want} missing in {kinds:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_mod_01_moderator_without_write_access_moderates_and_others_cannot() {
    let w = world().await;
    // `mod` holds Role::Moderator only: no collaborator role on the repo.
    let eff = w
        .env
        .forge
        .orgs
        .effective_role(&support::cx("mod"), &w.repo, "mod")
        .await
        .unwrap();
    assert_eq!(eff.role, RepoRole::Read as i32);
    let t = w.tok("mod");
    w.svc
        .lock_conversation(req(
            &t,
            LockConversationRequest {
                subject: Some(subj(SubjectKind::Issue, "3")),
                reason: ModerationLockReason::TooHeated as i32,
            },
        ))
        .await
        .unwrap();
    w.svc
        .block_user(req(
            &t,
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "spammer".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    // A token without `triage` cannot moderate even for a moderator.
    let read_only = w.env.token("mod", actions(&[Action::Read]));
    let r = w
        .svc
        .block_user(req(
            &read_only,
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "newcomer".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "POLICY_DENIED");
    // The locked conversation lets the moderator write, a stranger not.
    let locked =
        |who: &str| wc(who, &w.repo, WriteKind::Comment).subject(subj(SubjectKind::Issue, "3"));
    assert!(w.m.check_write(&locked("mod")).await.unwrap().allow);
    assert_eq!(
        w.m.check_write(&locked("newcomer"))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Locked
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_mod_01_denied_moderation_is_audited() {
    let w = world().await;
    let r = w
        .svc
        .block_user(req(
            &w.tok("reader"),
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "newcomer".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "PERMISSION_DENIED");
    let entries = w.audit().await;
    let denial = entries
        .iter()
        .find(|e| e.action == "moderation.block")
        .expect("denial recorded");
    assert_eq!(denial.result, "PERMISSION_DENIED");
    assert_eq!(denial.actor.as_ref().unwrap().id, "reader");
    assert!(
        w.m.blocked(
            &support::cx("owner"),
            &bee_forge::enterprise::moderation::blocks::BlockTarget::Org(w.org.id.clone()),
            "newcomer"
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_mod_01_hidden_comments_keep_their_edit_history() {
    let w = world().await;
    w.add_comment("c1", "spammer", "version 3");
    let cx = support::cx("owner");
    let editor = Principal {
        id: "spammer".into(),
        kind: PrincipalKind::Human as i32,
        ..Default::default()
    };
    w.m.record_comment_edit(&cx, "c1", &editor, "version 1")
        .await
        .unwrap();
    w.clock.advance(1000);
    w.m.record_comment_edit(&cx, "c1", &editor, "version 2")
        .await
        .unwrap();
    let t = w.tok("mod");
    let c = w
        .svc
        .minimize_comment(req(
            &t,
            MinimizeCommentRequest {
                comment_id: "c1".into(),
                reason: MinimizedReason::Abuse as i32,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(c.edited, "edited flag comes from the edit records");
    assert_eq!(c.minimized_reason, MinimizedReason::Abuse as i32);
    let hist = w.m.edit_history(&cx, "c1").await.unwrap();
    assert_eq!(
        hist.iter()
            .map(|e| e.previous_body.as_str())
            .collect::<Vec<_>>(),
        vec!["version 1", "version 2"]
    );
    // More edits while hidden still land in the history; unhiding keeps all of it.
    w.m.record_comment_edit(&cx, "c1", &editor, "version 3")
        .await
        .unwrap();
    let c = w
        .svc
        .unminimize_comment(req(
            &t,
            UnminimizeCommentRequest {
                comment_id: "c1".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.minimized_reason, MinimizedReason::Unspecified as i32);
    assert!(c.edited);
    assert_eq!(w.m.edit_history(&cx, "c1").await.unwrap().len(), 3);
    // Unminimizing a visible comment is INVALID_STATE.
    let r = w
        .svc
        .unminimize_comment(req(
            &t,
            UnminimizeCommentRequest {
                comment_id: "c1".into(),
            },
        ))
        .await;
    assert_eq!(reason(r), "INVALID_STATE");
    // Decoration for the work module.
    let mut plain = Comment {
        id: "c1".into(),
        ..Default::default()
    };
    w.m.decorate_comment(&cx, &mut plain).await.unwrap();
    assert!(plain.edited);
    // An unknown comment is NOT_FOUND; a reader cannot minimize.
    let r = w
        .svc
        .minimize_comment(req(
            &t,
            MinimizeCommentRequest {
                comment_id: "nope".into(),
                reason: MinimizedReason::Spam as i32,
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    let r = w
        .svc
        .minimize_comment(req(
            &w.tok("reader"),
            MinimizeCommentRequest {
                comment_id: "c1".into(),
                reason: MinimizedReason::Spam as i32,
            },
        ))
        .await;
    assert_eq!(reason(r), "PERMISSION_DENIED");
}

// ---------------------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocks_expire_and_cover_the_whole_delegation_chain() {
    let w = world().await;
    let owner = w.tok("owner");
    w.svc
        .block_user(req(
            &owner,
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "newcomer".into(),
                reason: "cool off".into(),
                duration_seconds: 3600,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    // Every blocked kind of write fails with BLOCKED, including being assigned.
    for kind in [
        WriteKind::Comment,
        WriteKind::Reaction,
        WriteKind::Issue,
        WriteKind::ChangeRequest,
        WriteKind::Review,
        WriteKind::Assignment,
        WriteKind::Mention,
    ] {
        let e =
            w.m.check_write(&wc("newcomer", &w.repo, kind))
                .await
                .unwrap_err();
        assert_eq!(e.reason, ErrorReason::Blocked, "{kind:?}");
    }
    // An agent owned by the blocked principal is blocked too.
    let e =
        w.m.check_write(&wc_agent(
            "agent-a",
            "newcomer",
            &w.repo,
            WriteKind::Comment,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::Blocked);
    let check = |who: &str| {
        let t = w.tok("owner");
        let r = CheckUserBlockedRequest {
            scope: org_block("acme"),
            principal_id: who.into(),
        };
        let svc = w.svc.clone();
        async move {
            svc.check_user_blocked(req(&t, r))
                .await
                .unwrap()
                .into_inner()
        }
    };
    let c = check("newcomer").await;
    assert!(c.blocked);
    assert!(c.block.unwrap().expires_at.is_some());
    assert!(!check("writer").await.blocked);
    let listed = w
        .svc
        .list_blocked_users(req(
            &owner,
            ListBlockedUsersRequest {
                scope: org_block("acme"),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.blocks.len(), 1);

    // After the expiry the block is gone on read, and the sweeper deletes the row.
    w.clock.advance(3600 * 1000);
    assert!(
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Comment))
            .await
            .is_ok()
    );
    assert!(!check("newcomer").await.blocked);
    let swept = w.m.sweep_expired(&Cx::system()).await.unwrap();
    assert_eq!(swept.blocks, 1);
    let r = w
        .svc
        .unblock_user(req(
            &owner,
            UnblockUserRequest {
                scope: org_block("acme"),
                principal_id: "newcomer".into(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    let kinds = w.event_kinds().await;
    assert!(
        kinds
            .iter()
            .filter(|k| *k == "moderation.unblocked")
            .count()
            >= 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn personal_blocks_apply_to_the_blockers_conversations() {
    let w = world().await;
    // `writer` blocks `spammer` personally; owner blocks nobody.
    w.svc
        .block_user(req(
            &w.tok("writer"),
            BlockUserRequest {
                scope: personal_block(),
                principal_id: "spammer".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let in_writers_issue = wc("spammer", &w.repo, WriteKind::Comment).subject_author("writer");
    let e = w.m.check_write(&in_writers_issue).await.unwrap_err();
    assert_eq!(e.reason, ErrorReason::Blocked);
    // Elsewhere the personal block does not apply.
    assert!(
        w.m.check_write(&wc("spammer", &w.repo, WriteKind::Comment))
            .await
            .is_ok()
    );
    // The repo owner's personal block applies to the whole repo.
    let e =
        w.m.check_write(&wc("spammer", &w.repo, WriteKind::Issue).repo_owner("writer"))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::Blocked);
    // Lists are personal: the owner sees none, the writer sees one.
    let list = |who: &str| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.list_blocked_users(req(
                &t,
                ListBlockedUsersRequest {
                    scope: personal_block(),
                    page: None,
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .blocks
            .len()
        }
    };
    assert_eq!(list("writer").await, 1);
    assert_eq!(list("owner").await, 0);
    // A user cannot block themselves, and org owners cannot be blocked by moderators.
    let r = w
        .svc
        .block_user(req(
            &w.tok("writer"),
            BlockUserRequest {
                scope: personal_block(),
                principal_id: "writer".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "INVALID_ARGUMENT");
    let r = w
        .svc
        .block_user(req(
            &w.tok("mod"),
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "owner".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "INVALID_STATE");
    // Unblock lifts it.
    w.svc
        .unblock_user(req(
            &w.tok("writer"),
            UnblockUserRequest {
                scope: personal_block(),
                principal_id: "spammer".into(),
            },
        ))
        .await
        .unwrap();
    assert!(w.m.check_write(&in_writers_issue).await.is_ok());
}

// ---------------------------------------------------------------------------------------
// Interaction limits
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limit_levels_judge_account_age_contributions_and_collaboration() {
    let w = world().await;
    w.standing.set_created("newcomer", T0 - HOUR);
    w.standing.set_created("reader", T0 - 3 * DAY);
    w.standing.set_created("spammer", T0 - 3 * DAY);
    w.standing.add_contribution(&w.repo.id, "spammer");
    let comment = |who: &str| wc(who, &w.repo, WriteKind::Comment);

    w.limit_repo(InteractionLimitLevel::ExistingUsers, false)
        .await;
    let e = w.m.check_write(&comment("newcomer")).await.unwrap_err();
    assert!(is_subcode(
        &e,
        ErrorReason::InteractionLimited,
        "existing_users"
    ));
    assert!(w.m.check_write(&comment("reader")).await.is_ok());
    // An account of unknown age fails closed.
    assert_eq!(
        w.m.check_write(&comment("outsider"))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InteractionLimited
    );

    w.limit_repo(InteractionLimitLevel::ContributorsOnly, false)
        .await;
    assert!(w.m.check_write(&comment("spammer")).await.is_ok());
    let e = w.m.check_write(&comment("reader")).await.unwrap_err();
    assert!(is_subcode(
        &e,
        ErrorReason::InteractionLimited,
        "contributors_only"
    ));

    w.limit_repo(InteractionLimitLevel::CollaboratorsOnly, false)
        .await;
    assert_eq!(
        w.m.check_write(&comment("spammer"))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InteractionLimited
    );
    for who in ["writer", "maint", "owner", "admin", "mod"] {
        assert!(
            w.m.check_write(&comment(who)).await.is_ok(),
            "{who} is never limited"
        );
    }
    // The caller may state collaborator standing instead of having it computed.
    assert!(
        w.m.check_write(&comment("newcomer").collaborator(true))
            .await
            .is_ok()
    );

    // Get reports the limit; it ends at its expiry.
    let get = || {
        let t = w.tok("reader");
        let svc = w.svc.clone();
        async move {
            svc.get_interaction_limit(req(
                &t,
                GetInteractionLimitRequest {
                    scope: lim_repo("acme/app"),
                },
            ))
            .await
            .unwrap()
            .into_inner()
        }
    };
    assert_eq!(
        get().await.level,
        InteractionLimitLevel::CollaboratorsOnly as i32
    );
    w.clock.advance(DAY);
    assert_eq!(get().await.level, InteractionLimitLevel::Unspecified as i32);
    assert!(w.m.check_write(&comment("newcomer")).await.is_ok());
    assert_eq!(w.m.sweep_expired(&Cx::system()).await.unwrap().limits, 1);

    // Remove: NOT_FOUND once nothing is in force.
    w.limit_repo(InteractionLimitLevel::ExistingUsers, false)
        .await;
    let rm = || {
        let t = w.tok("owner");
        let svc = w.svc.clone();
        async move {
            svc.remove_interaction_limit(req(
                &t,
                RemoveInteractionLimitRequest {
                    scope: lim_repo("acme/app"),
                },
            ))
            .await
        }
    };
    rm().await.unwrap();
    assert_eq!(reason(rm().await), "NOT_FOUND");
    assert!(w.m.check_write(&comment("newcomer")).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limits_judge_agents_by_their_owners_standing() {
    let w = world().await;
    w.standing.set_created("reader", T0 - 3 * DAY);
    let agent_in = |repo: &Repo| wc_agent("agent-a", "reader", repo, WriteKind::Comment);

    // applies_to_agents: judged by the owner's standing, not the agent's.
    w.limit_repo(InteractionLimitLevel::ContributorsOnly, true)
        .await;
    let e = w.m.check_write(&agent_in(&w.repo)).await.unwrap_err();
    assert_eq!(e.reason, ErrorReason::InteractionLimited);
    w.standing.add_contribution(&w.repo.id, "reader");
    assert!(
        w.m.check_write(&agent_in(&w.repo)).await.is_ok(),
        "the owner's contribution counts for its agents"
    );
    // The agent's own contributions do not help when the owner has none.
    w.standing.add_contribution(&w.repo.id, "agent-b");
    let e =
        w.m.check_write(&wc_agent(
            "agent-b",
            "newcomer",
            &w.repo,
            WriteKind::Comment,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InteractionLimited);

    // Without applies_to_agents agents are not limited at all.
    w.limit_repo(InteractionLimitLevel::CollaboratorsOnly, false)
        .await;
    assert!(
        w.m.check_write(&wc_agent(
            "agent-b",
            "newcomer",
            &w.repo,
            WriteKind::Comment
        ))
        .await
        .is_ok()
    );
    // ...while their human owner still is.
    assert_eq!(
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Comment))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InteractionLimited
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_limits_are_inherited_by_repos_and_the_strictest_wins() {
    let w = world().await;
    let owner = w.tok("owner");
    w.svc
        .set_interaction_limit(req(
            &owner,
            SetInteractionLimitRequest {
                scope: lim_org("acme"),
                level: InteractionLimitLevel::CollaboratorsOnly as i32,
                duration_seconds: 3600,
                applies_to_agents: true,
            },
        ))
        .await
        .unwrap();
    w.standing.set_created("reader", T0 - 3 * DAY);
    // A looser repo limit does not lift the org's.
    w.limit_repo(InteractionLimitLevel::ExistingUsers, true)
        .await;
    let e =
        w.m.check_write(&wc("reader", &w.other, WriteKind::Comment))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InteractionLimited);
    let got = w
        .svc
        .get_interaction_limit(req(
            &owner,
            GetInteractionLimitRequest {
                scope: lim_repo("acme/other"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.inherited_from, "org");
    assert_eq!(got.level, InteractionLimitLevel::CollaboratorsOnly as i32);
    // Time is on the org limit's side: after an hour only the repo limit remains.
    w.clock.advance(HOUR);
    assert!(
        w.m.check_write(&wc("reader", &w.repo, WriteKind::Comment))
            .await
            .is_ok()
    );
    // A user-scope limit applies to every repo that user owns.
    let user = Some(InteractionLimitScope {
        scope: Some(interaction_limit_scope::Scope::UserId("writer".into())),
    });
    w.svc
        .set_interaction_limit(req(
            &w.tok("writer"),
            SetInteractionLimitRequest {
                scope: user.clone(),
                level: InteractionLimitLevel::CollaboratorsOnly as i32,
                duration_seconds: 3600,
                applies_to_agents: false,
            },
        ))
        .await
        .unwrap();
    let e =
        w.m.check_write(&wc("newcomer", &w.other, WriteKind::Comment).repo_owner("writer"))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InteractionLimited);
    // Nobody else sets a user's limit.
    let r = w
        .svc
        .set_interaction_limit(req(
            &w.tok("reader"),
            SetInteractionLimitRequest {
                scope: user,
                level: InteractionLimitLevel::ExistingUsers as i32,
                duration_seconds: 3600,
                applies_to_agents: false,
            },
        ))
        .await;
    assert_eq!(reason(r), "PERMISSION_DENIED");
    // Bad input.
    for (level, secs) in [
        (InteractionLimitLevel::Unspecified, 3600),
        (InteractionLimitLevel::ExistingUsers, 0),
        (InteractionLimitLevel::ExistingUsers, 999_999_999),
    ] {
        let r = w
            .svc
            .set_interaction_limit(req(
                &owner,
                SetInteractionLimitRequest {
                    scope: lim_repo("acme/app"),
                    level: level as i32,
                    duration_seconds: secs,
                    applies_to_agents: false,
                },
            ))
            .await;
        assert_eq!(reason(r), "INVALID_ARGUMENT");
    }
}

// ---------------------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locks_stop_non_collaborators_but_not_collaborators_or_moderators() {
    let w = world().await;
    let t = w.tok("mod");
    let lock = |kind: SubjectKind, id: &str| LockConversationRequest {
        subject: Some(subj(kind, id)),
        reason: ModerationLockReason::OffTopic as i32,
    };
    let locked = w
        .svc
        .lock_conversation(req(&t, lock(SubjectKind::Issue, "7")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(locked.locked_by.unwrap().id, "mod");
    // Locking twice is INVALID_STATE; only conversations can be locked.
    assert_eq!(
        reason(
            w.svc
                .lock_conversation(req(&t, lock(SubjectKind::Issue, "7")))
                .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.svc
                .lock_conversation(req(&t, lock(SubjectKind::Release, "1")))
                .await
        ),
        "INVALID_ARGUMENT"
    );

    let on = |who: &str, id: &str| {
        wc(who, &w.repo, WriteKind::Comment).subject(subj(SubjectKind::Issue, id))
    };
    for who in ["newcomer", "reader", "outsider", "spammer"] {
        let e = w.m.check_write(&on(who, "7")).await.unwrap_err();
        assert!(
            is_subcode(&e, ErrorReason::Locked, "conversation_locked"),
            "{who}"
        );
    }
    for who in ["writer", "maint", "owner", "admin", "mod"] {
        assert!(w.m.check_write(&on(who, "7")).await.unwrap().allow, "{who}");
    }
    // Other conversations and other repos are not locked.
    assert!(w.m.check_write(&on("newcomer", "8")).await.is_ok());
    assert!(
        w.m.check_write(
            &wc("newcomer", &w.other, WriteKind::Comment).subject(subj(SubjectKind::Issue, "7"))
        )
        .await
        .is_ok()
    );
    // An agent is judged by its owner.
    let agent_on = |owner: &str| {
        wc_agent("agent-a", owner, &w.repo, WriteKind::Comment)
            .subject(subj(SubjectKind::Issue, "7"))
    };
    assert!(w.m.check_write(&agent_on("writer")).await.is_ok());
    assert_eq!(
        w.m.check_write(&agent_on("newcomer"))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Locked
    );
    // Unlock.
    w.svc
        .unlock_conversation(req(
            &t,
            UnlockConversationRequest {
                subject: Some(subj(SubjectKind::Issue, "7")),
            },
        ))
        .await
        .unwrap();
    assert!(w.m.check_write(&on("newcomer", "7")).await.is_ok());
    assert_eq!(
        reason(
            w.svc
                .unlock_conversation(req(
                    &t,
                    UnlockConversationRequest {
                        subject: Some(subj(SubjectKind::Issue, "7")),
                    },
                ))
                .await
        ),
        "INVALID_STATE"
    );
    // Writers cannot lock.
    assert_eq!(
        reason(
            w.svc
                .lock_conversation(req(&w.tok("writer"), lock(SubjectKind::Issue, "9")))
                .await
        ),
        "PERMISSION_DENIED"
    );
}

// ---------------------------------------------------------------------------------------
// Flood defenses
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_open_cr_caps_are_per_principal_and_per_owner_and_per_repo() {
    let w = world().await;
    let limits = w
        .set_flood(SetFloodLimitsRequest {
            max_open_change_requests_per_principal: Some(3),
            max_open_change_requests_per_owner: Some(5),
            ..Default::default()
        })
        .await;
    assert_eq!(limits.max_open_change_requests_per_principal, 3);
    assert_eq!(limits.throttle_duration_seconds, 3600, "default throttle");
    let cx_a = agent_cx("agent-a", "newcomer");
    let cx_b = agent_cx("agent-b", "newcomer");
    let cr = |id: &str, repo: &Repo| wc_agent(id, "newcomer", repo, WriteKind::ChangeRequest);
    for _ in 0..3 {
        w.m.check_write(&cr("agent-a", &w.repo)).await.unwrap();
        w.m.note_open(&cx_a, &w.repo.id, "agent-a", "newcomer")
            .await
            .unwrap();
    }
    let e = w.m.check_write(&cr("agent-a", &w.repo)).await.unwrap_err();
    assert!(is_subcode(
        &e,
        ErrorReason::ResourceExhausted,
        "max_open_change_requests_per_principal"
    ));
    assert!(e.retry_after_ms > 0 && e.retryable());
    // Repo B is unaffected for the same agent, however many it opens there.
    for _ in 0..10 {
        w.m.check_write(&cr("agent-a", &w.other)).await.unwrap();
        w.m.note_open(&cx_a, &w.other.id, "agent-a", "newcomer")
            .await
            .unwrap();
    }
    // The owner's cap covers all its agents.
    for _ in 0..2 {
        w.m.check_write(&cr("agent-b", &w.repo)).await.unwrap();
        w.m.note_open(&cx_b, &w.repo.id, "agent-b", "newcomer")
            .await
            .unwrap();
    }
    let e = w.m.check_write(&cr("agent-b", &w.repo)).await.unwrap_err();
    assert_eq!(e.subcode, "max_open_change_requests_per_owner");
    assert_eq!(
        w.m.open_count(&cx_a, &w.repo.id, "newcomer", true)
            .await
            .unwrap(),
        5
    );
    // Closing a CR frees a slot; counters never go below zero.
    w.m.note_closed(&cx_a, &w.repo.id, "agent-a", "newcomer")
        .await
        .unwrap();
    assert!(w.m.check_write(&cr("agent-b", &w.repo)).await.is_ok());
    for _ in 0..10 {
        w.m.note_closed(&cx_a, &w.repo.id, "agent-a", "newcomer")
            .await
            .unwrap();
    }
    assert_eq!(
        w.m.open_count(&cx_a, &w.repo.id, "agent-a", false)
            .await
            .unwrap(),
        0
    );
    // Comments are not subject to the CR caps.
    assert!(
        w.m.check_write(&wc_agent(
            "agent-b",
            "newcomer",
            &w.repo,
            WriteKind::Comment
        ))
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_rate_limit_throttles_releases_and_expires() {
    let w = world().await;
    w.set_flood(SetFloodLimitsRequest {
        max_new_change_requests_per_hour_per_owner: Some(4),
        throttle_duration_seconds: Some(3700),
        ..Default::default()
    })
    .await;
    let cr = |repo: &Repo| wc_agent("agent-a", "newcomer", repo, WriteKind::ChangeRequest);
    for i in 0..4 {
        w.clock.advance(1000);
        w.m.check_write(&cr(&w.repo))
            .await
            .unwrap_or_else(|e| panic!("CR {i}: {e:?}"));
    }
    let e = w.m.check_write(&cr(&w.repo)).await.unwrap_err();
    assert!(is_subcode(
        &e,
        ErrorReason::ResourceExhausted,
        "max_new_change_requests_per_hour_per_owner"
    ));
    assert_eq!(e.retry_after_ms, 3_700_000, "throttle outlasts the window");
    // The attacked repo shows the throttle; repo B and other kinds elsewhere are free.
    let list = |repo: &str| {
        let t = w.tok("owner");
        let r = ListThrottledPrincipalsRequest {
            repo: Some(rr(repo)),
            page: None,
        };
        let svc = w.svc.clone();
        async move {
            svc.list_throttled_principals(req(&t, r))
                .await
                .unwrap()
                .into_inner()
                .principals
        }
    };
    let throttled = list("acme/app").await;
    assert_eq!(throttled.len(), 1);
    assert_eq!(throttled[0].principal.as_ref().unwrap().id, "agent-a");
    assert_eq!(throttled[0].owner_id, "newcomer");
    assert_eq!(throttled[0].recent_count, 4);
    assert_eq!(
        throttled[0].limit,
        "max_new_change_requests_per_hour_per_owner"
    );
    assert!(list("acme/other").await.is_empty());
    for _ in 0..20 {
        w.m.check_write(&cr(&w.other)).await.unwrap();
    }
    // While throttled, the principal's other creations in the repo wait too, with a hint.
    let e =
        w.m.check_write(&wc_agent(
            "agent-a",
            "newcomer",
            &w.repo,
            WriteKind::Comment,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "throttled");
    assert!(e.retry_after_ms > 3_000_000 && e.retry_after_ms <= 3_700_000);
    // evaluate() reports the refusal as a decision with the throttle end.
    let d = w.m.evaluate(&cr(&w.repo)).await.unwrap();
    assert!(!d.allow);
    assert!(d.throttled_until.is_some());
    // The helper answers the same without recording anything.
    let f = w.m.flood_decision(&cr(&w.repo)).await.unwrap();
    assert!(!f.allowed && f.retry_after_ms > 0);

    // Early release by a maintainer.
    let release = |who: &str| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.release_throttled_principal(req(
                &t,
                ReleaseThrottledPrincipalRequest {
                    repo: Some(rr("acme/app")),
                    principal_id: "agent-a".into(),
                },
            ))
            .await
        }
    };
    assert_eq!(reason(release("writer").await), "PERMISSION_DENIED");
    release("maint").await.unwrap();
    assert_eq!(reason(release("maint").await), "NOT_FOUND");
    assert!(list("acme/app").await.is_empty());
    w.m.check_write(&cr(&w.repo)).await.unwrap();

    // Trip again, then let the throttle expire with the clock.
    for _ in 0..3 {
        w.m.check_write(&cr(&w.repo)).await.unwrap();
    }
    assert!(w.m.check_write(&cr(&w.repo)).await.is_err());
    w.clock.advance(3_699_000);
    assert!(
        w.m.check_write(&cr(&w.repo)).await.is_err(),
        "still throttled just before expiry"
    );
    w.clock.advance(2000);
    assert!(
        list("acme/app").await.is_empty(),
        "expired throttles are not listed"
    );
    let swept = w.m.sweep_expired(&Cx::system()).await.unwrap();
    assert_eq!(swept.throttles, 1);
    assert!(swept.activity >= 4);
    w.m.check_write(&cr(&w.repo)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_rate_window_slides_and_hints_when_a_slot_frees() {
    let w = world().await;
    w.set_flood(SetFloodLimitsRequest {
        max_new_comments_per_hour_per_owner: Some(2),
        max_new_issues_per_hour_per_owner: Some(1),
        throttle_duration_seconds: Some(0),
        ..Default::default()
    })
    .await;
    let comment = || wc("newcomer", &w.repo, WriteKind::Comment);
    w.m.check_write(&comment()).await.unwrap();
    w.clock.advance(20 * 60 * 1000);
    w.m.check_write(&comment()).await.unwrap();
    // No throttle record: the refusal says when the oldest admission leaves the window.
    let e = w.m.check_write(&comment()).await.unwrap_err();
    assert_eq!(e.reason, ErrorReason::ResourceExhausted);
    assert_eq!(e.retry_after_ms, 40 * 60 * 1000);
    // Dry runs do not record, so they never exhaust the window.
    w.clock.advance(e.retry_after_ms);
    for _ in 0..5 {
        w.m.check_write(&comment().dry_run()).await.unwrap();
    }
    w.m.check_write(&comment()).await.unwrap();
    // Issues have their own bucket, and users are counted separately.
    w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Issue))
        .await
        .unwrap();
    assert_eq!(
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Issue))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::ResourceExhausted
    );
    w.m.check_write(&wc("reader", &w.repo, WriteKind::Issue))
        .await
        .unwrap();
    let (throttled, _) =
        w.m.throttled_principals(&Cx::system(), &w.repo.id, "", 10)
            .await
            .unwrap();
    assert!(
        throttled.is_empty(),
        "no throttle records without a duration"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_collaborators_can_be_exempt_and_reputation_throttles() {
    let w = world().await;
    w.set_flood(SetFloodLimitsRequest {
        max_new_comments_per_hour_per_owner: Some(1),
        exempt_collaborators: Some(true),
        throttle_duration_seconds: Some(600),
        ..Default::default()
    })
    .await;
    for _ in 0..5 {
        w.m.check_write(&wc("writer", &w.repo, WriteKind::Comment))
            .await
            .unwrap();
    }
    w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Comment))
        .await
        .unwrap();
    assert_eq!(
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Comment))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::ResourceExhausted
    );

    // Reputation: a deterministic score from landed, rejected and spam outcomes.
    let sys = Cx::system();
    let org = &w.org.id;
    assert_eq!(w.m.reputation(&sys, "reader").await.unwrap(), 0.5);
    w.m.record_outcome(&sys, org, "reader", Outcome::Landed)
        .await
        .unwrap();
    w.m.record_outcome(&sys, org, "reader", Outcome::Landed)
        .await
        .unwrap();
    let s =
        w.m.record_outcome(&sys, org, "reader", Outcome::Rejected)
            .await
            .unwrap();
    assert_eq!(s, 3.0 / 5.0);
    let s =
        w.m.record_outcome(&sys, org, "spammer", Outcome::Spam)
            .await
            .unwrap();
    assert_eq!(s, 1.0 / 5.0);
    w.set_flood(SetFloodLimitsRequest {
        max_new_comments_per_hour_per_owner: Some(0),
        min_reputation: Some(0.3),
        ..Default::default()
    })
    .await;
    let e =
        w.m.check_write(&wc("spammer", &w.repo, WriteKind::Issue))
            .await
            .unwrap_err();
    assert!(is_subcode(
        &e,
        ErrorReason::ResourceExhausted,
        "min_reputation"
    ));
    assert_eq!(e.retry_after_ms, 600_000);
    w.m.check_write(&wc("reader", &w.repo, WriteKind::Issue))
        .await
        .unwrap();
    // An agent is held to the lowest reputation of its chain.
    let e =
        w.m.check_write(&wc_agent("agent-a", "spammer", &w.repo, WriteKind::Comment))
            .await
            .unwrap_err();
    assert_eq!(e.subcode, "min_reputation");
    // Out of range values are refused.
    let r = w
        .svc
        .set_flood_limits(req(
            &w.tok("owner"),
            SetFloodLimitsRequest {
                repo: Some(rr("acme/app")),
                min_reputation: Some(1.5),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "INVALID_ARGUMENT");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_limits_unset_fields_stay_and_defaults_apply() {
    let w = world().await;
    let got = w
        .svc
        .get_flood_limits(req(
            &w.tok("maint"),
            GetFloodLimitsRequest {
                repo: Some(rr("acme/app")),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        got.max_open_change_requests_per_principal, 0,
        "off by default"
    );
    w.set_flood(SetFloodLimitsRequest {
        max_open_change_requests_per_principal: Some(2),
        ..Default::default()
    })
    .await;
    let l = w
        .set_flood(SetFloodLimitsRequest {
            max_new_issues_per_hour_per_owner: Some(9),
            ..Default::default()
        })
        .await;
    assert_eq!(l.max_open_change_requests_per_principal, 2);
    assert_eq!(l.max_new_issues_per_hour_per_owner, 9);
    // Readers cannot read the flood settings.
    let r = w
        .svc
        .get_flood_limits(req(
            &w.tok("reader"),
            GetFloodLimitsRequest {
                repo: Some(rr("acme/app")),
            },
        ))
        .await;
    assert_eq!(reason(r), "PERMISSION_DENIED");

    // A configured default applies to repos without limits of their own.
    let clock = Arc::new(ManualClock::new(T0));
    let w2 = world_on(Arc::new(MemoryStore::new()), clock).await;
    let cfg =
        ModerationConfig::new(w2.standing.clone()).with_default_flood(ModerationFloodLimits {
            max_open_change_requests_per_principal: 1,
            throttle_duration_seconds: 60,
            ..Default::default()
        });
    let m2 = Moderation::new(w2.env.forge.clone(), cfg);
    let cx = agent_cx("agent-a", "newcomer");
    m2.note_open(&cx, &w2.repo.id, "agent-a", "newcomer")
        .await
        .unwrap();
    let e = m2
        .check_write(&wc_agent(
            "agent-a",
            "newcomer",
            &w2.repo,
            WriteKind::ChangeRequest,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::ResourceExhausted);
}

// ---------------------------------------------------------------------------------------
// First-time contributors
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_time_contributors_get_a_decision_not_an_error() {
    let w = world().await;
    w.standing.set_created("newcomer", T0 - HOUR);
    w.standing.set_created("reader", T0 - 10 * DAY);
    w.standing.add_contribution(&w.repo.id, "spammer");
    let cr = |who: &str| wc(who, &w.repo, WriteKind::ChangeRequest);

    // Defaults: label and approve runs, hold nothing.
    let d = w.m.check_write(&cr("newcomer")).await.unwrap();
    assert!(d.allow);
    assert_eq!(d.labels, vec!["first-time-contributor"]);
    assert!(!d.hold && d.require_run_approval);
    // Prior contributors and collaborators are not first-timers.
    for who in ["spammer", "writer", "owner", "mod"] {
        let d = w.m.check_write(&cr(who)).await.unwrap();
        assert!(
            d.labels.is_empty() && !d.hold && !d.require_run_approval,
            "{who}: {d:?}"
        );
    }
    // Issues are labelled but never held; comments are not touched.
    let d =
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Issue))
            .await
            .unwrap();
    assert_eq!(d.labels.len(), 1);
    assert!(!d.hold && !d.require_run_approval);
    let d =
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Comment))
            .await
            .unwrap();
    assert!(d.labels.is_empty());

    // Org settings apply to its repos; the repo's override them.
    let owner = w.tok("owner");
    let set_org = SetFirstTimeContributorSettingsRequest {
        org: "acme".into(),
        hold_change_requests: Some(true),
        label: Some("new-here".into()),
        ..Default::default()
    };
    let s = w
        .svc
        .set_first_time_contributor_settings(req(&owner, set_org))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(s.org, "acme");
    assert!(s.hold_change_requests);
    let d = w.m.check_write(&cr("newcomer")).await.unwrap();
    assert!(d.hold);
    assert_eq!(d.labels, vec!["new-here"]);
    let s = w
        .svc
        .set_first_time_contributor_settings(req(
            &w.tok("mod"),
            SetFirstTimeContributorSettingsRequest {
                repo: Some(rr("acme/app")),
                hold_change_requests: Some(false),
                label_first_contributions: Some(false),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !s.hold_change_requests && s.label == "new-here",
        "starts from the org's"
    );
    let d = w.m.check_write(&cr("newcomer")).await.unwrap();
    assert!(!d.hold && d.labels.is_empty());
    // The other repo still follows the org.
    let d =
        w.m.check_write(&wc("newcomer", &w.other, WriteKind::ChangeRequest))
            .await
            .unwrap();
    assert!(d.hold);
    let got = w
        .svc
        .get_first_time_contributor_settings(req(
            &w.tok("reader"),
            GetFirstTimeContributorSettingsRequest {
                repo: Some(rr("acme/other")),
                org: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(got.hold_change_requests);

    // Run approval policies.
    let set_run = |policy: ModerationRunApproval| {
        let t = w.tok("owner");
        let svc = w.svc.clone();
        async move {
            svc.set_first_time_contributor_settings(req(
                &t,
                SetFirstTimeContributorSettingsRequest {
                    repo: Some(rr("acme/app")),
                    run_approval: Some(policy as i32),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
        }
    };
    set_run(ModerationRunApproval::NewAccounts).await;
    assert!(
        w.m.check_write(&cr("newcomer"))
            .await
            .unwrap()
            .require_run_approval
    );
    assert!(
        !w.m.check_write(&cr("reader"))
            .await
            .unwrap()
            .require_run_approval,
        "an old account is not a new one"
    );
    set_run(ModerationRunApproval::AllOutside).await;
    assert!(
        w.m.check_write(&cr("spammer"))
            .await
            .unwrap()
            .require_run_approval,
        "every outside contributor, first time or not"
    );
    assert!(
        !w.m.check_write(&cr("writer"))
            .await
            .unwrap()
            .require_run_approval
    );
    set_run(ModerationRunApproval::FirstTime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_time_agents_are_judged_by_owner_unless_configured() {
    let w = world().await;
    w.standing.add_contribution(&w.repo.id, "reader");
    let cr = || wc_agent("agent-a", "reader", &w.repo, WriteKind::ChangeRequest);
    // The owner has contributed: the agent is not a first-timer.
    assert!(w.m.check_write(&cr()).await.unwrap().labels.is_empty());
    // Unless new agents are treated as first-time contributors in their own right.
    w.svc
        .set_first_time_contributor_settings(req(
            &w.tok("owner"),
            SetFirstTimeContributorSettingsRequest {
                repo: Some(rr("acme/app")),
                treat_new_agents_as_first_time: Some(true),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let d = w.m.check_write(&cr()).await.unwrap();
    assert_eq!(d.labels, vec!["first-time-contributor"]);
    w.standing.add_contribution(&w.repo.id, "agent-a");
    assert!(w.m.check_write(&cr()).await.unwrap().labels.is_empty());
}

// ---------------------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_lifecycle_and_queue_permissions() {
    let w = world().await;
    w.add_comment("c1", "spammer", "buy now");
    let report = |who: &str, subject: SubjectRef, audience: ModerationReportAudience| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.report_content(req(
                &t,
                ReportContentRequest {
                    subject: Some(subject),
                    category: ModerationReportCategory::Spam as i32,
                    audience: audience as i32,
                    body: "this is spam".into(),
                },
            ))
            .await
        }
    };
    // Readers (here: an outsider of a public repo) report to the maintainers.
    let r1 = report(
        "outsider",
        subj(SubjectKind::Comment, "c1"),
        ModerationReportAudience::Unspecified,
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(r1.state, ModerationReportState::Open as i32);
    assert_eq!(r1.audience, ModerationReportAudience::Maintainers as i32);
    assert_eq!(r1.reporter.as_ref().unwrap().id, "outsider");
    assert_eq!(r1.content_author.as_ref().unwrap().id, "spammer");
    // Reporting needs read access: a private repo is NOT_FOUND to outsiders.
    let secret = SubjectRef {
        kind: SubjectKind::Repo as i32,
        repo: Some(rr("acme/secret")),
        id: w.private.id.clone(),
    };
    assert_eq!(
        reason(
            report(
                "outsider",
                secret.clone(),
                ModerationReportAudience::Maintainers
            )
            .await
        ),
        "NOT_FOUND"
    );
    // A member can report it, and may address the site admins instead.
    let r2 = report("reader", secret, ModerationReportAudience::SiteAdmins)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r2.audience, ModerationReportAudience::SiteAdmins as i32);

    // Queues: repo and org for staff, site for site admins.
    let list =
        |who: &str, queue: list_content_reports_request::Queue, state: ModerationReportState| {
            let t = w.tok(who);
            let svc = w.svc.clone();
            async move {
                svc.list_content_reports(req(
                    &t,
                    ListContentReportsRequest {
                        queue: Some(queue),
                        state: state as i32,
                        ..Default::default()
                    },
                ))
                .await
            }
        };
    use list_content_reports_request::Queue;
    let open = ModerationReportState::Open;
    let n = |r: Result<tonic::Response<ListContentReportsResponse>, tonic::Status>| {
        r.unwrap().into_inner().reports.len()
    };
    assert_eq!(n(list("mod", Queue::Repo(rr("acme/app")), open).await), 1);
    assert_eq!(n(list("maint", Queue::Repo(rr("acme/app")), open).await), 1);
    assert_eq!(n(list("owner", Queue::Org("acme".into()), open).await), 1);
    assert_eq!(n(list("siteadmin", Queue::Site(true), open).await), 1);
    assert_eq!(
        n(list("siteadmin", Queue::Repo(rr("acme/app")), open).await),
        1
    );
    for who in ["writer", "reader", "outsider"] {
        assert_eq!(
            reason(list(who, Queue::Repo(rr("acme/app")), open).await),
            "PERMISSION_DENIED",
            "{who}"
        );
    }
    assert_eq!(
        reason(list("mod", Queue::Site(true), open).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        n(list(
            "mod",
            Queue::Org("acme".into()),
            ModerationReportState::Unspecified
        )
        .await),
        1
    );

    // Reading one report: its reporter and staff may, others may not.
    let get = |who: &str, id: &str| {
        let t = w.tok(who);
        let id = id.to_string();
        let svc = w.svc.clone();
        async move {
            svc.get_content_report(req(&t, GetContentReportRequest { id }))
                .await
        }
    };
    assert_eq!(
        get("outsider", &r1.id).await.unwrap().into_inner().id,
        r1.id
    );
    assert_eq!(get("mod", &r1.id).await.unwrap().into_inner().id, r1.id);
    assert_eq!(reason(get("reader", &r1.id).await), "PERMISSION_DENIED");
    assert_eq!(reason(get("mod", "01NOSUCHREPORT").await), "NOT_FOUND");
    assert_eq!(
        get("siteadmin", &r2.id).await.unwrap().into_inner().id,
        r2.id
    );

    // Resolving: staff only; a second resolution is INVALID_STATE; the action is recorded.
    let resolve = |who: &str, id: &str, state: ModerationReportState| {
        let t = w.tok(who);
        let id = id.to_string();
        let svc = w.svc.clone();
        async move {
            svc.resolve_content_report(req(
                &t,
                ResolveContentReportRequest {
                    id,
                    state: state as i32,
                    action: ModerationReportAction::ContentMinimized as i32,
                    note: "hidden".into(),
                },
            ))
            .await
        }
    };
    assert_eq!(
        reason(resolve("writer", &r1.id, ModerationReportState::Resolved).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(resolve("mod", &r1.id, ModerationReportState::Open).await),
        "INVALID_ARGUMENT"
    );
    let done = resolve("mod", &r1.id, ModerationReportState::Resolved)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.state, ModerationReportState::Resolved as i32);
    assert_eq!(done.action, ModerationReportAction::ContentMinimized as i32);
    assert_eq!(done.resolution_note, "hidden");
    assert_eq!(done.resolved_by.unwrap().id, "mod");
    assert!(done.resolved_at.is_some());
    assert_eq!(
        reason(resolve("mod", &r1.id, ModerationReportState::Dismissed).await),
        "INVALID_STATE"
    );
    assert_eq!(n(list("mod", Queue::Repo(rr("acme/app")), open).await), 0);
    assert_eq!(
        n(list(
            "mod",
            Queue::Repo(rr("acme/app")),
            ModerationReportState::Resolved
        )
        .await),
        1
    );
    let dismissed = resolve("siteadmin", &r2.id, ModerationReportState::Dismissed)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dismissed.state, ModerationReportState::Dismissed as i32);
    // Both the filing and the resolutions are audited.
    let actions: Vec<String> = w.audit().await.into_iter().map(|e| e.action).collect();
    assert_eq!(
        actions.iter().filter(|a| *a == "moderation.report").count(),
        2
    );
    assert!(actions.iter().any(|a| a == "moderation.report_resolve"));
}

// ---------------------------------------------------------------------------------------
// Moderators
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moderators_are_role_members_listed_principals_or_team_members() {
    let w = world().await;
    let add = |who: &str, m: add_moderator_request::Moderator| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.add_moderator(req(
                &t,
                AddModeratorRequest {
                    org: "acme".into(),
                    moderator: Some(m),
                },
            ))
            .await
        }
    };
    use add_moderator_request::Moderator as M;
    // A listed principal that is not even an org member can moderate; it gets no role.
    let m = add("owner", M::PrincipalId("mod2".into()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(m.org, "acme");
    assert!(matches!(
        m.moderator,
        Some(moderation_moderator::Moderator::Principal(p)) if p.id == "mod2"
    ));
    assert_eq!(
        reason(add("owner", M::PrincipalId("mod2".into())).await),
        "ALREADY_EXISTS"
    );
    assert!(
        w.env
            .forge
            .orgs
            .member(&support::cx("owner"), &w.org.id, "mod2")
            .await
            .unwrap()
            .is_none()
    );
    let lock = |who: &str, id: &str| {
        let t = w.tok(who);
        let id = id.to_string();
        let svc = w.svc.clone();
        async move {
            svc.lock_conversation(req(
                &t,
                LockConversationRequest {
                    subject: Some(subj(SubjectKind::Issue, &id)),
                    reason: ModerationLockReason::Spam as i32,
                },
            ))
            .await
        }
    };
    lock("mod2", "1").await.unwrap();
    // Moderators cannot add moderators; owners and admins can.
    assert_eq!(
        reason(add("mod", M::PrincipalId("newcomer".into())).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(add("mod2", M::PrincipalId("newcomer".into())).await),
        "PERMISSION_DENIED"
    );
    add("admin", M::PrincipalId("agent-b".into()))
        .await
        .unwrap();

    // Teams: the team must belong to the org; its members moderate.
    w.teams
        .teams
        .write()
        .unwrap()
        .insert("team-1".into(), (w.org.id.clone(), vec!["reader".into()]));
    w.teams.teams.write().unwrap().insert(
        "team-x".into(),
        ("01OTHERORG".into(), vec!["outsider".into()]),
    );
    assert_eq!(reason(lock("reader", "2").await), "PERMISSION_DENIED");
    add("owner", M::TeamId("team-1".into())).await.unwrap();
    lock("reader", "2").await.unwrap();
    assert_eq!(
        reason(add("owner", M::TeamId("team-x".into())).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(add("owner", M::TeamId("team-none".into())).await),
        "NOT_FOUND"
    );
    let cx = support::cx("owner");
    assert!(w.m.is_moderator(&cx, &w.org.id, "reader").await.unwrap());
    assert!(
        w.m.is_moderator(&cx, &w.org.id, "mod").await.unwrap(),
        "role member"
    );
    assert!(!w.m.is_moderator(&cx, &w.org.id, "outsider").await.unwrap());
    // A moderator through a team also counts at the write path (locks).
    let on =
        |who: &str| wc(who, &w.repo, WriteKind::Comment).subject(subj(SubjectKind::Issue, "2"));
    assert_eq!(
        w.m.check_write(&on("outsider")).await.unwrap_err().reason,
        ErrorReason::Locked
    );
    assert!(w.m.check_write(&on("reader")).await.is_ok());

    // Listing shows listed principals, teams and role members.
    let list = w
        .svc
        .list_moderators(req(
            &w.tok("reader"),
            ListModeratorsRequest {
                org: "acme".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let names: Vec<String> = list
        .moderators
        .iter()
        .map(|m| match &m.moderator {
            Some(moderation_moderator::Moderator::Principal(p)) => p.id.clone(),
            Some(moderation_moderator::Moderator::TeamId(t)) => format!("team:{t}"),
            None => String::new(),
        })
        .collect();
    for want in ["mod2", "agent-b", "team:team-1", "mod", "agent"] {
        assert!(names.contains(&want.to_string()), "{want} in {names:?}");
    }
    assert_eq!(
        reason(
            w.svc
                .list_moderators(req(
                    &w.tok("outsider"),
                    ListModeratorsRequest {
                        org: "acme".into(),
                        page: None,
                    },
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );

    // Removing takes the power away.
    let remove = |who: &str, m: remove_moderator_request::Moderator| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.remove_moderator(req(
                &t,
                RemoveModeratorRequest {
                    org: "acme".into(),
                    moderator: Some(m),
                },
            ))
            .await
        }
    };
    use remove_moderator_request::Moderator as R;
    remove("owner", R::PrincipalId("mod2".into()))
        .await
        .unwrap();
    assert_eq!(reason(lock("mod2", "3").await), "PERMISSION_DENIED");
    assert_eq!(
        reason(remove("owner", R::PrincipalId("mod2".into())).await),
        "NOT_FOUND"
    );
    remove("owner", R::TeamId("team-1".into())).await.unwrap();
    assert_eq!(reason(lock("reader", "4").await), "PERMISSION_DENIED");
}

// ---------------------------------------------------------------------------------------
// Takedowns
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takedowns_disable_a_repo_or_comment_until_reinstated() {
    let w = world().await;
    let repo_subject = SubjectRef {
        kind: SubjectKind::Repo as i32,
        repo: Some(rr("acme/app")),
        id: String::new(),
    };
    let create = |who: &str, subject: SubjectRef| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.create_takedown(req(
                &t,
                CreateTakedownRequest {
                    subject: Some(subject),
                    kind: TakedownKind::Dmca as i32,
                    notice_text: "infringes".into(),
                    notice_url: "https://example.test/notice/1".into(),
                    complainant: "Rights Holder".into(),
                },
            ))
            .await
        }
    };
    // Only site admins and the org's owners take down.
    for who in ["admin", "mod", "writer", "reader", "outsider", "maint"] {
        assert_eq!(
            reason(create(who, repo_subject.clone()).await),
            "PERMISSION_DENIED",
            "{who}"
        );
    }
    let t = create("owner", repo_subject.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(t.state, TakedownState::Active as i32);
    assert_eq!(t.created_by.as_ref().unwrap().id, "owner");
    assert!(!t.audit_entry_id.is_empty());
    assert_eq!(t.subject.as_ref().unwrap().id, w.repo.id);
    assert_eq!(
        reason(create("siteadmin", repo_subject.clone()).await),
        "ALREADY_EXISTS"
    );
    // The audit chain holds the entry the takedown names.
    w.env.forge.flush().await.unwrap();
    let entry = w
        .env
        .forge
        .audit
        .get(
            &Cx::system(),
            &org_scope(&w.org.id),
            &EntryKey::Id(t.audit_entry_id.clone()),
        )
        .await
        .unwrap();
    assert_eq!(entry.action, "moderation.takedown_create");

    // Writes fail with PERMISSION_DENIED / taken_down; other repos are fine.
    assert!(w.m.is_taken_down(&repo_subject).await.unwrap());
    assert!(w.m.is_repo_taken_down(&w.repo.id).await.unwrap());
    let e =
        w.m.check_write(&wc("writer", &w.repo, WriteKind::Comment))
            .await
            .unwrap_err();
    assert!(is_subcode(&e, ErrorReason::PermissionDenied, "taken_down"));
    assert!(
        w.m.check_write(&wc("writer", &w.other, WriteKind::Comment))
            .await
            .is_ok()
    );
    assert!(
        !w.m.is_taken_down(&SubjectRef {
            kind: SubjectKind::Repo as i32,
            repo: Some(rr("acme/other")),
            id: String::new(),
        })
        .await
        .unwrap()
    );

    // Comments can be taken down on their own.
    let c = create("siteadmin", subj(SubjectKind::Comment, "c9"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        c.subject.as_ref().unwrap().kind,
        SubjectKind::Comment as i32
    );
    let on = |id: &str| {
        wc("writer", &w.other, WriteKind::Reaction).subject(SubjectRef {
            kind: SubjectKind::Comment as i32,
            repo: None,
            id: id.into(),
        })
    };
    assert!(is_subcode(
        &w.m.check_write(&on("c9")).await.unwrap_err(),
        ErrorReason::PermissionDenied,
        "taken_down"
    ));
    assert!(w.m.check_write(&on("c10")).await.is_ok());

    // Listing: owners see their repo's, site admins everything.
    let list = |who: &str, repo: Option<RepoRef>, state: TakedownState| {
        let t = w.tok(who);
        let svc = w.svc.clone();
        async move {
            svc.list_takedowns(req(
                &t,
                ListTakedownsRequest {
                    repo,
                    state: state as i32,
                    ..Default::default()
                },
            ))
            .await
        }
    };
    let count = |r: Result<tonic::Response<ListTakedownsResponse>, tonic::Status>| {
        r.unwrap().into_inner().takedowns.len()
    };
    assert_eq!(
        count(list("owner", Some(rr("acme/app")), TakedownState::Unspecified).await),
        2
    );
    assert_eq!(
        count(list("siteadmin", None, TakedownState::Active).await),
        2
    );
    assert_eq!(
        reason(list("owner", None, TakedownState::Unspecified).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(list("writer", Some(rr("acme/app")), TakedownState::Unspecified).await),
        "PERMISSION_DENIED"
    );
    let get = |who: &str, id: &str| {
        let t = w.tok(who);
        let id = id.to_string();
        let svc = w.svc.clone();
        async move { svc.get_takedown(req(&t, GetTakedownRequest { id })).await }
    };
    assert_eq!(get("owner", &t.id).await.unwrap().into_inner().id, t.id);
    assert_eq!(reason(get("writer", &t.id).await), "PERMISSION_DENIED");
    assert_eq!(reason(get("owner", "01NOSUCH").await), "NOT_FOUND");

    // Reinstating: only when active; writes pass again.
    let reinstate = |who: &str, id: &str| {
        let t = w.tok(who);
        let id = id.to_string();
        let svc = w.svc.clone();
        async move {
            svc.reinstate_takedown(req(
                &t,
                ReinstateTakedownRequest {
                    id,
                    reason: "counter-notice".into(),
                },
            ))
            .await
        }
    };
    assert_eq!(reason(reinstate("admin", &t.id).await), "PERMISSION_DENIED");
    let r = reinstate("owner", &t.id).await.unwrap().into_inner();
    assert_eq!(r.state, TakedownState::Reinstated as i32);
    assert_eq!(r.reinstate_reason, "counter-notice");
    assert_eq!(r.reinstated_by.unwrap().id, "owner");
    assert_eq!(reason(reinstate("owner", &t.id).await), "INVALID_STATE");
    assert!(
        w.m.check_write(&wc("writer", &w.repo, WriteKind::Comment))
            .await
            .is_ok()
    );
    assert!(!w.m.is_taken_down(&repo_subject).await.unwrap());
    assert_eq!(
        count(list("siteadmin", None, TakedownState::Reinstated).await),
        1
    );
    // A reinstated subject can be taken down again.
    create("siteadmin", repo_subject).await.unwrap();
    let actions: Vec<String> = w.audit().await.into_iter().map(|e| e.action).collect();
    assert!(actions.iter().any(|a| a == "moderation.takedown_reinstate"));
}

// ---------------------------------------------------------------------------------------
// Service surface
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_permission_matrix() {
    let w = world().await;
    let who = [
        "owner", "admin", "mod", "maint", "writer", "reader", "outsider",
    ];
    let staff = ["owner", "admin", "mod"];
    let repo_staff = ["owner", "admin", "mod", "maint"];
    let org_admins = ["owner", "admin"];
    for (n, p) in who.iter().enumerate() {
        let t = w.tok(p);
        let r = w
            .svc
            .block_user(req(
                &t,
                BlockUserRequest {
                    scope: org_block("acme"),
                    principal_id: "spammer".into(),
                    ..Default::default()
                },
            ))
            .await;
        check_matrix("block_user", p, &staff, r.map(|_| ()));
        let r = w
            .svc
            .set_interaction_limit(req(
                &t,
                SetInteractionLimitRequest {
                    scope: lim_repo("acme/app"),
                    level: InteractionLimitLevel::ExistingUsers as i32,
                    duration_seconds: 60,
                    applies_to_agents: false,
                },
            ))
            .await;
        check_matrix("set_interaction_limit", p, &repo_staff, r.map(|_| ()));
        let r = w
            .svc
            .lock_conversation(req(
                &t,
                LockConversationRequest {
                    subject: Some(subj(SubjectKind::Issue, &format!("m{n}"))),
                    reason: ModerationLockReason::Resolved as i32,
                },
            ))
            .await;
        check_matrix("lock_conversation", p, &repo_staff, r.map(|_| ()));
        let r = w
            .svc
            .set_flood_limits(req(
                &t,
                SetFloodLimitsRequest {
                    repo: Some(rr("acme/app")),
                    max_new_issues_per_hour_per_owner: Some(100),
                    ..Default::default()
                },
            ))
            .await;
        check_matrix("set_flood_limits", p, &repo_staff, r.map(|_| ()));
        let r = w
            .svc
            .list_content_reports(req(
                &t,
                ListContentReportsRequest {
                    queue: Some(list_content_reports_request::Queue::Repo(rr("acme/app"))),
                    ..Default::default()
                },
            ))
            .await;
        check_matrix("list_content_reports", p, &repo_staff, r.map(|_| ()));
        let r = w
            .svc
            .set_first_time_contributor_settings(req(
                &t,
                SetFirstTimeContributorSettingsRequest {
                    org: "acme".into(),
                    label: Some("x".into()),
                    ..Default::default()
                },
            ))
            .await;
        check_matrix("set_first_time_settings(org)", p, &staff, r.map(|_| ()));
        let r = w
            .svc
            .add_moderator(req(
                &t,
                AddModeratorRequest {
                    org: "acme".into(),
                    moderator: Some(add_moderator_request::Moderator::PrincipalId(format!(
                        "tmp{n}"
                    ))),
                },
            ))
            .await;
        check_matrix("add_moderator", p, &org_admins, r.map(|_| ()));
        let r = w
            .svc
            .create_takedown(req(
                &t,
                CreateTakedownRequest {
                    subject: Some(subj(SubjectKind::Comment, &format!("tc{n}"))),
                    kind: TakedownKind::Abuse as i32,
                    ..Default::default()
                },
            ))
            .await;
        check_matrix("create_takedown", p, &["owner"], r.map(|_| ()));
    }
    // On a private repo a stranger does not even learn that the repo exists.
    let r = w
        .svc
        .set_flood_limits(req(
            &w.tok("outsider"),
            SetFloodLimitsRequest {
                repo: Some(rr("acme/secret")),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    // Unauthenticated calls and unknown repos.
    let st = w
        .svc
        .list_codes_of_conduct(tonic::Request::new(ListCodesOfConductRequest {}))
        .await
        .unwrap_err();
    assert_eq!(st.code(), tonic::Code::Unauthenticated);
    let r = w
        .svc
        .get_flood_limits(req(
            &w.tok("owner"),
            GetFloodLimitsRequest {
                repo: Some(rr("acme/nope")),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
}

fn check_matrix(op: &str, who: &str, allowed: &[&str], r: Result<(), tonic::Status>) {
    let ok = r.is_ok();
    let want = allowed.contains(&who);
    let why = r
        .err()
        .and_then(|s| bee_proto::errors::detail_from_status(&s))
        .map(|d| d.reason);
    assert_eq!(
        ok, want,
        "{op} by {who}: allowed={ok}, expected {want} ({why:?})"
    );
    if !ok {
        assert_eq!(why.as_deref(), Some("PERMISSION_DENIED"), "{op} by {who}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn community_endpoints_codes_of_conduct_and_served_elsewhere() {
    let w = world().await;
    let codes = w
        .svc
        .list_codes_of_conduct(req(&w.tok("outsider"), ListCodesOfConductRequest {}))
        .await
        .unwrap()
        .into_inner()
        .codes_of_conduct;
    assert!(codes.len() >= 2);
    let mut keys: Vec<&str> = codes.iter().map(|c| c.key.as_str()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), codes.len(), "unique keys");
    assert!(
        codes
            .iter()
            .all(|c| !c.body.is_empty() && !c.name.is_empty())
    );
    let st = w
        .svc
        .get_moderation_community_profile(req(
            &w.tok("owner"),
            GetModerationCommunityProfileRequest {
                repo: Some(rr("acme/app")),
                revision: None,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(st.code(), tonic::Code::Unimplemented);
    let d = bee_proto::errors::detail_from_status(&st).unwrap();
    assert_eq!(d.reason, "UNIMPLEMENTED");
    assert_eq!(d.subcode, "served_elsewhere");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_moderation_action_is_audited_and_emits_an_event() {
    let w = world().await;
    w.add_comment("c1", "spammer", "spam");
    let owner = w.tok("owner");
    let siteadmin = w.tok("siteadmin");
    // One call per state-changing operation.
    w.svc
        .block_user(req(
            &owner,
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "spammer".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    w.svc
        .unblock_user(req(
            &owner,
            UnblockUserRequest {
                scope: org_block("acme"),
                principal_id: "spammer".into(),
            },
        ))
        .await
        .unwrap();
    let report = w
        .svc
        .report_content(req(
            &w.tok("reader"),
            ReportContentRequest {
                subject: Some(subj(SubjectKind::Comment, "c1")),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    w.svc
        .resolve_content_report(req(
            &owner,
            ResolveContentReportRequest {
                id: report.id,
                state: ModerationReportState::Resolved as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    w.limit_repo(InteractionLimitLevel::ExistingUsers, false)
        .await;
    w.svc
        .remove_interaction_limit(req(
            &owner,
            RemoveInteractionLimitRequest {
                scope: lim_repo("acme/app"),
            },
        ))
        .await
        .unwrap();
    w.svc
        .minimize_comment(req(
            &owner,
            MinimizeCommentRequest {
                comment_id: "c1".into(),
                reason: MinimizedReason::Spam as i32,
            },
        ))
        .await
        .unwrap();
    w.svc
        .unminimize_comment(req(
            &owner,
            UnminimizeCommentRequest {
                comment_id: "c1".into(),
            },
        ))
        .await
        .unwrap();
    let lock = SubjectRef {
        kind: SubjectKind::ChangeRequest as i32,
        repo: Some(rr("acme/app")),
        id: "5".into(),
    };
    w.svc
        .lock_conversation(req(
            &owner,
            LockConversationRequest {
                subject: Some(lock.clone()),
                reason: ModerationLockReason::Resolved as i32,
            },
        ))
        .await
        .unwrap();
    w.svc
        .unlock_conversation(req(
            &owner,
            UnlockConversationRequest {
                subject: Some(lock),
            },
        ))
        .await
        .unwrap();
    w.svc
        .add_moderator(req(
            &owner,
            AddModeratorRequest {
                org: "acme".into(),
                moderator: Some(add_moderator_request::Moderator::PrincipalId("mod2".into())),
            },
        ))
        .await
        .unwrap();
    w.svc
        .remove_moderator(req(
            &owner,
            RemoveModeratorRequest {
                org: "acme".into(),
                moderator: Some(remove_moderator_request::Moderator::PrincipalId(
                    "mod2".into(),
                )),
            },
        ))
        .await
        .unwrap();
    w.svc
        .set_first_time_contributor_settings(req(
            &owner,
            SetFirstTimeContributorSettingsRequest {
                repo: Some(rr("acme/app")),
                hold_change_requests: Some(true),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    w.set_flood(SetFloodLimitsRequest {
        max_new_change_requests_per_hour_per_owner: Some(1),
        ..Default::default()
    })
    .await;
    let cr = || wc("newcomer", &w.repo, WriteKind::ChangeRequest);
    w.m.check_write(&cr()).await.unwrap();
    assert!(w.m.check_write(&cr()).await.is_err());
    w.svc
        .release_throttled_principal(req(
            &owner,
            ReleaseThrottledPrincipalRequest {
                repo: Some(rr("acme/app")),
                principal_id: "newcomer".into(),
            },
        ))
        .await
        .unwrap();
    w.m.record_outcome(&Cx::system(), &w.org.id, "newcomer", Outcome::Landed)
        .await
        .unwrap();
    let td = w
        .svc
        .create_takedown(req(
            &siteadmin,
            CreateTakedownRequest {
                subject: Some(SubjectRef {
                    kind: SubjectKind::Repo as i32,
                    repo: Some(rr("acme/other")),
                    id: String::new(),
                }),
                kind: TakedownKind::Legal as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    w.svc
        .reinstate_takedown(req(
            &siteadmin,
            ReinstateTakedownRequest {
                id: td.id,
                reason: "error".into(),
            },
        ))
        .await
        .unwrap();

    let entries = w.audit().await;
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    for want in [
        "moderation.block",
        "moderation.unblock",
        "moderation.report",
        "moderation.report_resolve",
        "moderation.limit_set",
        "moderation.limit_remove",
        "moderation.minimize",
        "moderation.unminimize",
        "moderation.lock",
        "moderation.unlock",
        "moderation.moderator_add",
        "moderation.moderator_remove",
        "moderation.first_time_settings_set",
        "moderation.flood_limits_set",
        "moderation.throttle",
        "moderation.throttle_release",
        "moderation.reputation",
        "moderation.takedown_create",
        "moderation.takedown_reinstate",
    ] {
        assert!(actions.contains(&want), "{want} missing in {actions:?}");
    }
    assert!(entries.iter().all(|e| e.result == "ok"));
    let kinds = w.event_kinds().await;
    for want in [
        "moderation.blocked",
        "moderation.unblocked",
        "moderation.reported",
        "moderation.report_resolved",
        "moderation.limit_set",
        "moderation.limit_removed",
        "moderation.comment_minimized",
        "moderation.comment_unminimized",
        "moderation.conversation_locked",
        "moderation.conversation_unlocked",
        "moderation.moderator_added",
        "moderation.moderator_removed",
        "moderation.first_time_settings_set",
        "moderation.flood_limits_set",
        "moderation.principal_throttled",
        "moderation.throttle_released",
        "moderation.reputation_changed",
        "moderation.takedown_created",
        "moderation.takedown_reinstated",
    ] {
        assert!(
            kinds.iter().any(|k| k == want),
            "{want} missing in {kinds:?}"
        );
    }
    // Every event kind the module emits is registered.
    let reg = Registry::core().unwrap();
    for k in &kinds {
        assert!(reg.event_kind(k).is_some(), "{k} not registered");
    }
    let v = w
        .env
        .forge
        .audit
        .verify(&Cx::system(), &org_scope(&w.org.id), 0, 0, &[])
        .await
        .unwrap();
    assert!(v.ok, "{v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unconfigured_repo_is_never_restricted_and_check_writes_nothing() {
    let w = world().await;
    for kind in [
        WriteKind::Comment,
        WriteKind::Reaction,
        WriteKind::Issue,
        WriteKind::ChangeRequest,
        WriteKind::Review,
        WriteKind::Assignment,
        WriteKind::Mention,
        WriteKind::Fork,
        WriteKind::Other,
    ] {
        for who in ["newcomer", "outsider", "writer"] {
            let d = w.m.check_write(&wc(who, &w.repo, kind)).await.unwrap();
            assert!(d.allow && d.throttled_until.is_none() && d.denial.is_none());
        }
    }
    let (throttled, _) =
        w.m.throttled_principals(&Cx::system(), &w.repo.id, "", 10)
            .await
            .unwrap();
    assert!(throttled.is_empty());
    // No audit entry or event came out of the checks themselves.
    assert!(w.audit().await.is_empty());
    assert!(w.event_kinds().await.is_empty());
}

// ---------------------------------------------------------------------------------------
// Stores
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_rate_limit_holds_under_concurrent_writes() {
    let w = world().await;
    w.set_flood(SetFloodLimitsRequest {
        max_new_comments_per_hour_per_owner: Some(5),
        throttle_duration_seconds: Some(0),
        ..Default::default()
    })
    .await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let m = w.m.clone();
        let c = wc("newcomer", &w.repo, WriteKind::Comment);
        tasks.push(tokio::spawn(async move { m.check_write(&c).await.is_ok() }));
    }
    let mut admitted = 0;
    for t in tasks {
        if t.await.unwrap() {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 5, "exactly the hourly limit is admitted");
}

fn owner_cx() -> Cx {
    support::cx("owner")
}

/// A shortened tour of every moderation table, run on each backend.
async fn flow(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>) {
    let w = world_on(store, clock).await;
    w.add_comment("c1", "spammer", "spam");
    let owner = w.tok("owner");
    let sys = Cx::system();
    // Block + limit + lock.
    w.svc
        .block_user(req(
            &owner,
            BlockUserRequest {
                scope: org_block("acme"),
                principal_id: "spammer".into(),
                duration_seconds: 600,
                hide_content: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        w.m.check_write(&wc("spammer", &w.repo, WriteKind::Comment))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Blocked
    );
    assert!(
        w.m.comment_state(&sys, "c1").await.unwrap().is_some(),
        "hide_content minimized the comment"
    );
    w.limit_repo(InteractionLimitLevel::CollaboratorsOnly, true)
        .await;
    assert_eq!(
        w.m.check_write(&wc("newcomer", &w.repo, WriteKind::Issue))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InteractionLimited
    );
    w.svc
        .lock_conversation(req(
            &owner,
            LockConversationRequest {
                subject: Some(subj(SubjectKind::Issue, "1")),
                reason: ModerationLockReason::TooHeated as i32,
            },
        ))
        .await
        .unwrap();
    w.svc
        .remove_interaction_limit(req(
            &owner,
            RemoveInteractionLimitRequest {
                scope: lim_repo("acme/app"),
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        w.m.check_write(
            &wc("newcomer", &w.repo, WriteKind::Comment).subject(subj(SubjectKind::Issue, "1"))
        )
        .await
        .unwrap_err()
        .reason,
        ErrorReason::Locked
    );
    // Flood: counters, window, throttle, release.
    w.set_flood(SetFloodLimitsRequest {
        max_open_change_requests_per_principal: Some(1),
        max_new_change_requests_per_hour_per_owner: Some(2),
        ..Default::default()
    })
    .await;
    let cx = support::cx("reader");
    let cr = || wc("reader", &w.repo, WriteKind::ChangeRequest);
    w.m.check_write(&cr()).await.unwrap();
    w.m.note_open(&cx, &w.repo.id, "reader", "reader")
        .await
        .unwrap();
    assert_eq!(
        w.m.check_write(&cr()).await.unwrap_err().reason,
        ErrorReason::ResourceExhausted
    );
    w.m.note_closed(&cx, &w.repo.id, "reader", "reader")
        .await
        .unwrap();
    w.m.check_write(&cr()).await.unwrap();
    let e = w.m.check_write(&cr()).await.unwrap_err();
    assert!(e.retry_after_ms >= 3_600_000);
    w.m.release_throttle(&owner_cx(), &w.org.id, &w.repo.id, "reader")
        .await
        .unwrap();
    w.m.check_write(&cr()).await.unwrap();
    // Reports, takedowns, edits.
    let r = w
        .svc
        .report_content(req(
            &w.tok("reader"),
            ReportContentRequest {
                subject: Some(subj(SubjectKind::Comment, "c1")),
                category: ModerationReportCategory::Spam as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    w.svc
        .resolve_content_report(req(
            &owner,
            ResolveContentReportRequest {
                id: r.id.clone(),
                state: ModerationReportState::Resolved as i32,
                action: ModerationReportAction::UserBlocked as i32,
                note: String::new(),
            },
        ))
        .await
        .unwrap();
    let after = w
        .svc
        .get_content_report(req(&owner, GetContentReportRequest { id: r.id }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(after.state, ModerationReportState::Resolved as i32);
    let td = w
        .svc
        .create_takedown(req(
            &owner,
            CreateTakedownRequest {
                subject: Some(SubjectRef {
                    kind: SubjectKind::Repo as i32,
                    repo: Some(rr("acme/other")),
                    id: String::new(),
                }),
                kind: TakedownKind::Abuse as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(w.m.is_repo_taken_down(&w.other.id).await.unwrap());
    w.svc
        .reinstate_takedown(req(
            &owner,
            ReinstateTakedownRequest {
                id: td.id,
                reason: String::new(),
            },
        ))
        .await
        .unwrap();
    assert!(!w.m.is_repo_taken_down(&w.other.id).await.unwrap());
    let editor = Principal {
        id: "spammer".into(),
        ..Default::default()
    };
    w.m.record_comment_edit(&sys, "c1", &editor, "v1")
        .await
        .unwrap();
    assert_eq!(w.m.edit_history(&sys, "c1").await.unwrap().len(), 1);
    // Expiry and sweeping.
    w.clock.advance(7200 * 1000);
    let swept = w.m.sweep_expired(&sys).await.unwrap();
    assert_eq!(swept.blocks, 1);
    assert!(
        w.m.check_write(&wc("spammer", &w.repo, WriteKind::Reaction))
            .await
            .is_ok()
    );
    let v = w
        .env
        .forge
        .audit
        .verify(&sys, &org_scope(&w.org.id), 0, 0, &[])
        .await
        .unwrap();
    assert!(v.ok, "{v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moderation_runs_on_the_memory_store() {
    flow(Arc::new(MemoryStore::new()), Arc::new(ManualClock::new(T0))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moderation_runs_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(T0));
    let store = SqlStore::open(
        SqlConfig::sqlite(dir.path()),
        clock.clone() as Arc<dyn Clock>,
    )
    .await
    .unwrap();
    flow(Arc::new(store), clock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moderation_runs_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("moderation_{}", std::process::id());
    }
    let clock = Arc::new(ManualClock::new(T0));
    let store = SqlStore::open(cfg, clock.clone() as Arc<dyn Clock>)
        .await
        .unwrap();
    flow(Arc::new(store), clock).await;
}

#[test]
fn the_module_registers_one_surface_and_its_migration_is_numbered_from_one() {
    let reg = Registry::core().unwrap();
    let m = reg
        .modules()
        .iter()
        .find(|m| m.name == "moderation")
        .unwrap();
    assert_eq!(m.surfaces.len(), 1);
    assert_eq!(m.surfaces[0].name(), "moderation");
    let ids: Vec<String> = m.migrations.iter().map(|g| g.id()).collect();
    assert_eq!(ids, vec!["moderation/0001_create_moderation"]);
    assert!(reg.event_kind("moderation.blocked").is_some());
    assert!(reg.filter_known("moderation.*"));
}
