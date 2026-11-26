//! The organization stop applies at native and pull-request publication and holds the queue.

use std::time::Duration;

use base64::Engine as _;
use bee_conformance::load::client::{Client, RpcError, pid};
use bee_core::{ChangeId, ObjectId};
use bee_forge::Cx;
use bee_forge::audit::AuditDraft;
use bee_forge::changes::QueueStep;
use bee_proto::errors::detail_from_status;
use bee_proto::v1::{
    self, change_service_client::ChangeServiceClient, event_service_client::EventServiceClient,
    ref_service_client::RefServiceClient, repo_service_client::RepoServiceClient,
    safety_service_client::SafetyServiceClient,
};
use bee_store::faults::{Fault, FaultInjector, FaultRule};

use crate::scale_support::{main_tip, start_fresh};

async fn agent_writer(owner: &Client) -> Client {
    let id = owner
        .create_agent("merge worker")
        .await
        .expect("create agent");
    owner
        .add_member("dev", &id)
        .await
        .expect("add to organization");
    owner
        .add_writer(&id)
        .await
        .expect("grant repository write access");
    let token = owner
        .mint_token(&id, &[], 0, 0)
        .await
        .expect("mint agent credential");
    owner.with_token(token)
}

async fn agent_maintainer(owner: &Client) -> Client {
    let agent = agent_writer(owner).await;
    let (principal_id, _) = agent.who_am_i().await.expect("agent identity");
    RepoServiceClient::new(owner.channel().clone())
        .add_collaborator(owner.req(v1::AddCollaboratorRequest {
            repo: owner.repo(),
            principal_id,
            role: v1::RepoRole::Maintain as i32,
            ..Default::default()
        }))
        .await
        .expect("grant ref administration");
    agent
}

async fn commit(client: &Client, base: &ObjectId, file: &str, seed: u8) -> ObjectId {
    client
        .commit(
            base,
            &[(
                file.to_string(),
                b"Updated instructions for the example\n".to_vec(),
            )],
            ChangeId::new([seed; 16]),
            "Update example documentation",
        )
        .await
        .expect("create changeset")
}

fn stop_request(owner: &Client, reason: &str) -> tonic::Request<v1::SetSafetyStopRequest> {
    owner.req(v1::SetSafetyStopRequest {
        org: "dev".into(),
        reason: reason.into(),
    })
}

fn assert_frozen(error: &RpcError) {
    let detail = error.detail.as_ref().expect("structured denial");
    assert_eq!(detail.reason, "POLICY_DENIED");
    assert_eq!(detail.subcode, "safety_merge_frozen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_fences_create_update_delete_and_rename_on_unprotected_refs() {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = &server.client;
    let agent = agent_maintainer(owner).await;
    let (base, _) = main_tip(owner).await;
    let head = commit(&agent, &base, "docs/frozen-work.md", 25).await;
    let branch = "freeze-example";
    let version = agent
        .create_ref(branch, &base)
        .await
        .expect("agent can create a ref");
    let mut safety = SafetyServiceClient::new(owner.channel().clone());
    safety
        .stop_everything(stop_request(owner, "Pause ref publication."))
        .await
        .expect("stop");

    assert_frozen(
        &agent
            .update_ref(branch, &head, version)
            .await
            .expect_err("frozen update"),
    );
    assert_frozen(
        &agent
            .create_ref("freeze-new", &head)
            .await
            .expect_err("frozen creation"),
    );
    let mut refs = RefServiceClient::new(owner.channel().clone());
    assert_frozen(&RpcError::from(
        refs.delete_ref(agent.req(v1::DeleteRefRequest {
            repo: agent.repo(),
            name: branch.into(),
            expected_version: version,
            ..Default::default()
        }))
        .await
        .expect_err("frozen deletion"),
    ));
    assert_frozen(&RpcError::from(
        refs.rename_ref(agent.req(v1::RenameRefRequest {
            repo: agent.repo(),
            name: branch.into(),
            new_name: "freeze-renamed".into(),
            expected_version: version,
            ..Default::default()
        }))
        .await
        .expect_err("frozen rename"),
    ));
    assert_eq!(owner.resolve(branch).await.unwrap(), Some((base, version)));
    assert_eq!(owner.resolve("freeze-new").await.unwrap(), None);
    assert_eq!(owner.resolve("freeze-renamed").await.unwrap(), None);
    let new_version = owner
        .update_ref(branch, &head, version)
        .await
        .expect("person can still update");
    assert_eq!(
        owner.resolve(branch).await.unwrap(),
        Some((head, new_version))
    );
    server.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_fences_git_receive_pack_on_an_unprotected_ref() {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = &server.client;
    let agent = agent_maintainer(owner).await;
    let (base, _) = main_tip(owner).await;
    let branch = "git-freeze-example";
    let sibling = "git-freeze-sibling";
    let version = owner.create_ref(branch, &base).await.unwrap();
    let sibling_version = owner.create_ref(sibling, &base).await.unwrap();
    let mut safety = SafetyServiceClient::new(owner.channel().clone());
    safety
        .stop_everything(stop_request(owner, "Pause git publication."))
        .await
        .expect("stop");

    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{}/dev/playground.git", server.srv.addr);
    let auth = format!(
        "http.extraHeader=Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("git:{}", agent.token()))
    );
    let work = dir.path().join("work");
    tokio::task::spawn_blocking(move || {
        let git = |cwd: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(cwd)
                .args([
                    "-c",
                    &auth,
                    "-c",
                    "user.name=Merge Worker",
                    "-c",
                    "user.email=merge@example.com",
                ])
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", dir.path().join("gitconfig"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .expect("git runs")
        };
        let cloned = git(dir.path(), &["clone", "-q", &url, "work"]);
        assert!(
            cloned.status.success(),
            "{}",
            String::from_utf8_lossy(&cloned.stderr)
        );
        std::fs::write(work.join("frozen.txt"), "A new Git revision\n").unwrap();
        let added = git(&work, &["add", "frozen.txt"]);
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
        let committed = git(&work, &["commit", "-m", "Update example"]);
        assert!(
            committed.status.success(),
            "{}",
            String::from_utf8_lossy(&committed.stderr)
        );
        for args in [
            vec!["push".into(), url.clone(), format!("HEAD:{branch}")],
            vec![
                "push".into(),
                "--atomic".into(),
                url,
                format!("HEAD:{branch}"),
                format!("HEAD:{sibling}"),
            ],
        ] {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let pushed = git(&work, &args);
            assert!(!pushed.status.success(), "the frozen push was accepted");
            let stderr = String::from_utf8_lossy(&pushed.stderr);
            assert!(
                stderr.contains("POLICY_DENIED: safety_merge_frozen"),
                "{stderr}"
            );
        }
    })
    .await
    .unwrap();
    assert_eq!(owner.resolve(branch).await.unwrap(), Some((base, version)));
    assert_eq!(
        owner.resolve(sibling).await.unwrap(),
        Some((base, sibling_version))
    );
    server.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_holds_human_enabled_auto_merge_of_an_agent_revision_until_resume() {
    assert_human_auto_merge_held_until_resume(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_holds_human_enabled_auto_merge_through_the_queue() {
    assert_human_auto_merge_held_until_resume(true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_holds_agent_authorship_when_a_person_pushes_the_revision() {
    assert_human_auto_merge_held_until_resume(false, false).await;
}

async fn assert_human_auto_merge_held_until_resume(merge_queue_required: bool, agent_pushed: bool) {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = &server.client;
    let agent = agent_writer(owner).await;
    let (base, version) = main_tip(owner).await;
    let original = commit(owner, &base, "docs/before-revision.md", 26).await;
    let revised = commit(&agent, &base, "docs/after-revision.md", 27).await;
    RepoServiceClient::new(owner.channel().clone())
        .update_repo_settings(owner.req(v1::UpdateRepoSettingsRequest {
            repo: owner.repo(),
            allow_auto_merge: Some(true),
            merge_queue_required: Some(merge_queue_required),
            ..Default::default()
        }))
        .await
        .expect("allow auto-merge");
    let mut changes = ChangeServiceClient::new(owner.channel().clone());
    let pull = changes
        .open_change(owner.req(v1::OpenChangeRequest {
            repo: owner.repo(),
            target_ref: "main".into(),
            head: Some(pid(&original)),
            title: "Revise the release instructions".into(),
            draft: true,
            ..Default::default()
        }))
        .await
        .expect("open a human-authored draft")
        .into_inner()
        .id;
    changes
        .enable_auto_merge(owner.req(v1::EnableAutoMergeRequest {
            repo: owner.repo(),
            id: pull.clone(),
            ..Default::default()
        }))
        .await
        .expect("enable while draft");
    let mut safety = SafetyServiceClient::new(owner.channel().clone());
    safety
        .stop_everything(stop_request(owner, "Pause automatic publication."))
        .await
        .expect("stop");
    let pusher = if agent_pushed { &agent } else { owner };
    let cr = changes
        .revise_change(pusher.req(v1::ReviseChangeRequest {
            repo: agent.repo(),
            id: pull.clone(),
            head: Some(pid(&revised)),
        }))
        .await
        .expect("record the new revision")
        .into_inner();
    assert_eq!(
        cr.revisions.last().unwrap().pusher.as_ref().unwrap().kind(),
        if agent_pushed {
            v1::PrincipalKind::Agent
        } else {
            v1::PrincipalKind::Human
        }
    );
    changes
        .mark_change_ready(owner.req(v1::MarkChangeReadyRequest {
            repo: owner.repo(),
            id: pull.clone(),
        }))
        .await
        .expect("mark the revision ready");

    let forge = server.srv.forge.as_ref().expect("forge");
    let cx = Cx::system();
    let repo = forge
        .forge
        .orgs
        .repo_any(&cx, &server.repo().0.to_text())
        .await
        .unwrap();
    assert!(
        forge
            .changes
            .try_auto_merge(&repo, &pull)
            .await
            .unwrap()
            .is_none()
    );
    let cr = changes
        .get_change(owner.req(v1::GetChangeRequest {
            repo: owner.repo(),
            id: pull.clone(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cr.state(), v1::ChangeState::Open);
    assert!(cr.auto_merge.is_some());
    assert_eq!(main_tip(owner).await, (base, version));

    let mut events = EventServiceClient::new(owner.channel().clone())
        .subscribe(owner.req(v1::SubscribeRequest {
            repo: owner.repo(),
            kinds: vec!["cr.landed".into()],
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    safety
        .resume_everything(stop_request(owner, "Resume automatic publication."))
        .await
        .expect("resume");
    let event = tokio::time::timeout(Duration::from_secs(10), events.message())
        .await
        .expect("resume wakes auto-merge")
        .unwrap()
        .unwrap();
    assert_eq!(event.subject.unwrap().id, pull);
    assert_eq!(main_tip(owner).await.1, version + 1);
    drop(events);
    server.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_publication_fence_leaves_agents_writes_free_and_stop_drains_the_cas() {
    let faults = FaultInjector::new();
    let (server, _dir) = start_fresh(|_| {}, Some(faults.clone())).await;
    let owner = &server.client;
    let agent = agent_writer(owner).await;
    let (base, version) = main_tip(owner).await;
    let first = commit(&agent, &base, "docs/in-flight.md", 28).await;
    let second = commit(&agent, &base, "docs/after-stop.md", 29).await;
    faults.add(FaultRule::always(
        "meta.update_ref",
        Fault::Delay(Duration::from_secs(3)),
    ));
    let before = faults.hits_of("meta.update_ref");
    let landing = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.land("main", &first).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while faults.hits_of("meta.update_ref") == before {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("land reached the delayed CAS");

    let cx = Cx::system();
    let forge = &server.srv.forge.as_ref().unwrap().forge;
    let repo = forge
        .orgs
        .repo_any(&cx, &server.repo().0.to_text())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut tx = forge
            .db
            .begin(bee_forge::agents::SURFACE, &cx)
            .await
            .unwrap();
        tx.audit(AuditDraft::new("test.safety_publication", &repo.org_id))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    })
    .await
    .expect("agents-store write completes during publication");
    assert!(!landing.is_finished(), "the land must still be in progress");
    assert_eq!(main_tip(owner).await, (base, version));

    let mut safety = SafetyServiceClient::new(owner.channel().clone());
    safety
        .stop_everything(stop_request(owner, "Drain the active publication."))
        .await
        .expect("stop");
    let tip_at_stop = main_tip(owner).await;
    assert_eq!(
        tip_at_stop.1,
        version + 1,
        "stop waited for the CAS to complete"
    );
    landing
        .await
        .unwrap()
        .expect("the in-flight agent land succeeded before stop");
    assert_frozen(
        &agent
            .land("main", &second)
            .await
            .expect_err("no later agent publication"),
    );
    assert_eq!(main_tip(owner).await, tip_at_stop);
    server.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_unavailable_forge_refuses_agent_publication_and_disabled_forge_allows_it() {
    for enabled in [true, false] {
        let (server, _dir) = start_fresh(
            |cfg| {
                cfg.forge.enabled = enabled;
                cfg.forge.database = "sqlite:/dev/null/forge".into();
            },
            None,
        )
        .await;
        assert!(server.srv.forge.is_none());
        let owner = &server.client;
        let agent_id = owner.create_agent("publication worker").await.unwrap();
        let agent = owner.with_token(owner.mint_token(&agent_id, &[], 0, 0).await.unwrap());
        let (base, version) = main_tip(owner).await;
        let head = commit(&agent, &base, "docs/outage.md", 30).await;
        let workspace = format!("ws/{agent_id}/outage-example");
        let direct = agent.create_ref(&workspace, &head).await;
        let land = agent.land("main", &head).await;
        if enabled {
            let errors = [
                direct.expect_err("ref publication must fail closed"),
                land.expect_err("land must fail closed"),
            ];
            for error in errors {
                let detail = error.detail.expect("structured error");
                assert_eq!(detail.reason, "UNAVAILABLE");
                assert_eq!(detail.subcode, "safety_forge_unavailable");
            }
            assert_eq!(agent.resolve(&workspace).await.unwrap(), None);
            assert_eq!(main_tip(owner).await, (base, version));
            owner
                .create_ref("outage-example", &head)
                .await
                .expect("person can still publish");
        } else {
            direct.expect("pure Bee allows agent ref publication");
            land.expect("pure Bee allows agent land");
        }
        server.srv.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_blocks_agent_native_and_pull_request_lands_until_resume() {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = server.client.with_timeout(Duration::from_secs(30));
    let agent = agent_writer(&owner).await;
    let (base, version) = main_tip(&owner).await;
    let native = commit(&agent, &base, "docs/native.md", 21).await;
    let pull_head = commit(&agent, &base, "docs/pull-request.md", 22).await;
    let pull = agent
        .open_change("main", &pull_head, "Update pull request instructions")
        .await
        .expect("open pull request");
    let mut safety = SafetyServiceClient::new(owner.channel().clone());

    let denied = safety
        .stop_everything(stop_request(
            &agent,
            "The agent cannot stop the organization.",
        ))
        .await
        .expect_err("only people who own an organization can stop it");
    assert_eq!(
        detail_from_status(&denied)
            .expect("structured denial")
            .reason,
        "PERMISSION_DENIED"
    );

    let stop = safety
        .stop_everything(stop_request(&owner, "Pause to check a release."))
        .await
        .expect("freeze merges")
        .into_inner();
    assert!(stop.frozen);
    assert_frozen(
        &agent
            .land("main", &native)
            .await
            .expect_err("native agent land refused"),
    );
    assert_frozen(
        &agent
            .land_change(&pull)
            .await
            .expect_err("agent pull request refused"),
    );
    assert_eq!(main_tip(&owner).await, (base, version));

    let human_head = commit(&owner, &base, "docs/release.md", 23).await;
    let human_pull = owner
        .open_change("main", &human_head, "Update release instructions")
        .await
        .expect("open human pull request");
    owner
        .land_change(&human_pull)
        .await
        .expect("human can still merge");
    assert_eq!(main_tip(&owner).await.1, version + 1);

    let resume = safety
        .resume_everything(stop_request(&owner, "Release check finished."))
        .await
        .expect("unfreeze merges")
        .into_inner();
    assert!(!resume.frozen);
    agent
        .land_change(&pull)
        .await
        .expect("agent pull request can merge again");
    agent
        .land("main", &native)
        .await
        .expect("agent native land can publish again");
    assert_eq!(main_tip(&owner).await.1, version + 3);
    server.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_resume_wakes_a_held_queue_on_a_nondefault_ref() {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = server.client.with_timeout(Duration::from_secs(30));
    let agent = agent_writer(&owner).await;
    let (base, _) = main_tip(&owner).await;
    owner
        .create_ref("release", &base)
        .await
        .expect("create release ref");
    RepoServiceClient::new(owner.channel().clone())
        .update_repo_settings(owner.req(v1::UpdateRepoSettingsRequest {
            repo: owner.repo(),
            merge_queue_required: Some(true),
            ..Default::default()
        }))
        .await
        .expect("require queue");
    let mut safety = SafetyServiceClient::new(owner.channel().clone());
    safety
        .stop_everything(stop_request(&owner, "Pause to check the release."))
        .await
        .expect("freeze merges");

    let head = commit(&agent, &base, "docs/release-queue.md", 24).await;
    let pull = agent
        .open_change("release", &head, "Update the release queue example")
        .await
        .expect("open queued pull request");
    let mut changes = ChangeServiceClient::new(owner.channel().clone());
    changes
        .enqueue_change(agent.req(v1::EnqueueChangeRequest {
            repo: owner.repo(),
            id: pull.clone(),
            ..Default::default()
        }))
        .await
        .expect("enqueue pull request as the agent");

    let forge = server.srv.forge.as_ref().expect("forge");
    let cx = Cx::system();
    let repo = forge
        .forge
        .orgs
        .repo_any(&cx, &server.repo().0.to_text())
        .await
        .expect("repository row");
    assert_eq!(
        forge
            .changes
            .step_queue(&cx, &repo, "release")
            .await
            .expect("step while frozen"),
        QueueStep::Waiting
    );
    let mut events = EventServiceClient::new(owner.channel().clone())
        .subscribe(owner.req(v1::SubscribeRequest {
            repo: owner.repo(),
            kinds: vec!["cr.landed".into()],
            ..Default::default()
        }))
        .await
        .expect("subscribe to merges")
        .into_inner();
    safety
        .resume_everything(stop_request(&owner, "Release check complete."))
        .await
        .expect("resume merges");

    let event = tokio::time::timeout(Duration::from_secs(10), events.message())
        .await
        .expect("resumption wakes the release ref")
        .expect("read merge event")
        .expect("release pull request was merged");
    assert_eq!(event.subject.expect("pull request subject").id, pull);
    assert!(
        changes
            .get_merge_queue(owner.req(v1::GetMergeQueueRequest {
                repo: owner.repo(),
                r#ref: "release".into(),
            }))
            .await
            .expect("read the queue")
            .into_inner()
            .entries
            .is_empty()
    );
    drop(events);
    server.srv.shutdown().await;
}
