//! Agents adopting followed upgrades end to end on a running server with the deterministic
//! runner (`forge.agent_runner = "fake"`; `docs/spec/agents.md` §5 "Feed triggers"):
//!
//! 1. Agent A, working in the private repo X, follows the public repo Y through a feed
//!    trigger (`SetAgentFeedTrigger`; A follows Y as itself).
//! 2. A post publishes on Y (a person's change lands with its post).
//! 3. One `ADOPT_EVALUATE` run is queued in X, exactly once, with the post, its change, the
//!    changed paths and the target repo as its context.
//! 4. The runner opens an issue in X whose `inspired_by` names the post.
//! 5. The post's adoption count rises, and `ListPostAdoptions` lists the issue.
//! 6. The adoption is in a private repo: a signed-out reader and an agent outside the org see
//!    no adoption.

use crate::scale_support;

use std::time::Duration;

use bee_conformance::load::client::{Client, change_id, pid};
use bee_proto::v1;
use bee_proto::v1::agent_service_client::AgentServiceClient;
use bee_proto::v1::change_service_client::ChangeServiceClient;
use bee_proto::v1::issue_service_client::IssueServiceClient;
use bee_proto::v1::post_service_client::PostServiceClient;
use bee_proto::v1::repo_service_client::RepoServiceClient;
use bee_proto::v1::user_service_client::UserServiceClient;
use scale_support::*;

const TARGET: &str = "dev/target";
const UPSTREAM: &str = "dev/upstream";

fn rref(name: &str) -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName(name.into())),
    })
}

async fn create_repo(s: &Srv, name: &str, vis: v1::Visibility) {
    RepoServiceClient::new(s.client.channel().clone())
        .create_repo(s.client.req(v1::CreateRepoRequest {
            org: "dev".into(),
            name: name.into(),
            visibility: vis as i32,
            auto_init: true,
            ..Default::default()
        }))
        .await
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
}

async fn adopt_runs(c: &Client) -> Vec<v1::AgentRun> {
    AgentServiceClient::new(c.channel().clone())
        .list_agent_runs(c.req(v1::ListAgentRunsRequest {
            repo: rref(TARGET),
            task_kind: v1::AgentTaskKind::AdoptEvaluate as i32,
            ..Default::default()
        }))
        .await
        .expect("list runs")
        .into_inner()
        .runs
}

async fn get_post(c: Option<&Client>, ch: &tonic::transport::Channel, id: &str) -> v1::Post {
    let msg = v1::GetPostRequest { post_id: id.into() };
    let r = match c {
        Some(c) => c.req(msg),
        None => tonic::Request::new(msg),
    };
    PostServiceClient::new(ch.clone())
        .get_post(r)
        .await
        .expect("get post")
        .into_inner()
}

async fn adoptions(c: &Client, id: &str) -> Vec<v1::PostAdoption> {
    PostServiceClient::new(c.channel().clone())
        .list_post_adoptions(c.req(v1::ListPostAdoptionsRequest {
            post_id: id.into(),
            page: None,
        }))
        .await
        .expect("list adoptions")
        .into_inner()
        .adoptions
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_adopts_a_followed_repos_upgrade_exactly_once() {
    let (s, _dir) = start_fresh(
        |c| {
            c.forge.agent_runner = "fake".into();
            c.forge.agent_debounce_ms = 300;
        },
        None,
    )
    .await;
    create_repo(&s, "target", v1::Visibility::Private).await;
    create_repo(&s, "upstream", v1::Visibility::Public).await;
    let dev = Client::new(s.client.channel().clone(), s.client.token(), TARGET);
    let dev_upstream = Client::new(s.client.channel().clone(), s.client.token(), UPSTREAM);
    let me = s.user();

    // The adopting agent: a member of the org, a writer of X, registered and owned by dev.
    let agent = s.client.create_agent("adopter").await.expect("agent");
    s.client.add_member("dev", &agent).await.expect("member");
    dev.add_writer(&agent).await.expect("writer");
    let agents = AgentServiceClient::new(s.client.channel().clone());
    agents
        .clone()
        .register_agent(dev.req(v1::RegisterAgentRequest {
            org: "dev".into(),
            agent_principal_id: agent.clone(),
            name: "adopter".into(),
            runtime: "claude-code".into(),
            toolsets: vec!["read".into(), "commit".into()],
            owners: vec![me.clone()],
            purpose: "adopt upgrades from followed repos".into(),
            ..Default::default()
        }))
        .await
        .expect("register agent");
    let agent_token = s
        .client
        .mint_token(&agent, &[], 0, 0)
        .await
        .expect("agent token");
    let as_agent = Client::new(s.client.channel().clone(), agent_token, TARGET);

    // 1. The feed trigger: A follows Y.
    let trigger = agents
        .clone()
        .set_agent_feed_trigger(dev.req(v1::SetAgentFeedTriggerRequest {
            repo: rref(TARGET),
            agent: agent.clone(),
            enabled: true,
            daily_usd_budget: 5.0,
            max_runs_per_day: 10,
            sources: vec![rref(UPSTREAM).unwrap()],
        }))
        .await
        .expect("set feed trigger")
        .into_inner();
    assert_eq!(trigger.sources, vec![rref(UPSTREAM).unwrap()]);
    assert!(trigger.enabled);
    let following = UserServiceClient::new(as_agent.channel().clone())
        .is_following_repo(as_agent.req(v1::IsFollowingRepoRequest {
            repo: rref(UPSTREAM),
        }))
        .await
        .expect("is following")
        .into_inner()
        .following;
    assert!(following, "the agent follows the source as itself");

    // 2. A person's change on Y lands with its post.
    let (tip, _) = main_tip(&dev_upstream).await;
    let head = dev_upstream
        .commit(
            &tip,
            &[("src/cache.rs".into(), b"layers\n".to_vec())],
            change_id(9101),
            "Cache the build layers",
        )
        .await
        .expect("commit");
    let cr = ChangeServiceClient::new(dev_upstream.channel().clone())
        .open_change(dev_upstream.req(v1::OpenChangeRequest {
            repo: rref(UPSTREAM),
            target_ref: "main".into(),
            head: Some(pid(&head)),
            title: "Cache the build layers".into(),
            post_text: "Builds are 3x faster #ci".into(),
            ..Default::default()
        }))
        .await
        .expect("open change")
        .into_inner();
    let post_id = cr.post.as_ref().expect("post draft").id.clone();
    dev_upstream.land_change(&cr.id).await.expect("land");
    let post = get_post(Some(&dev), s.client.channel(), &post_id).await;
    assert_eq!(post.state(), v1::PostState::Published);
    assert_eq!(post.counts.as_ref().unwrap().adoptions, 0);

    // 3. One ADOPT_EVALUATE run in X, finished.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let run = loop {
        let runs = adopt_runs(&dev).await;
        if let Some(r) = runs.iter().find(|r| {
            matches!(
                r.state(),
                v1::AgentRunState::Succeeded | v1::AgentRunState::Failed
            )
        }) {
            break r.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no adopt run finished; runs: {runs:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(run.state(), v1::AgentRunState::Succeeded, "{}", run.summary);
    assert_eq!(run.trigger, "feed");
    assert_eq!(run.task.as_ref().unwrap().post_id, post_id);
    assert_eq!(run.agent.as_ref().unwrap().id, agent);
    let events = agents
        .clone()
        .list_agent_run_events(dev.req(v1::ListAgentRunEventsRequest {
            run_id: run.id.clone(),
            kinds: vec![v1::AgentRunEventKind::Log as i32],
            ..Default::default()
        }))
        .await
        .expect("run events")
        .into_inner()
        .events;
    let context = events
        .iter()
        .map(|e| e.text.as_str())
        .find(|t| t.contains("## What to do"))
        .unwrap_or_else(|| panic!("no context log in {events:?}"));
    for want in [
        format!("Post {post_id} in {UPSTREAM}"),
        format!("Change #{}: Cache the build layers", cr.number),
        "Builds are 3x faster #ci".to_string(),
        "1 path changed".to_string(),
        "- src/cache.rs".to_string(),
        format!("{TARGET} (default ref main)"),
        "inspired_by_post_id".to_string(),
    ] {
        assert!(context.contains(&want), "missing {want:?} in\n{context}");
    }

    // 4. It opened an issue in X inspired by the post.
    let n: u64 = run
        .summary
        .strip_prefix("opened issue #")
        .and_then(|r| r.split_once(':'))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or_else(|| panic!("summary {:?}", run.summary));
    let issue = IssueServiceClient::new(dev.channel().clone())
        .get_issue_by_number(dev.req(v1::GetIssueByNumberRequest {
            repo: rref(TARGET),
            number: n,
            include_html: false,
        }))
        .await
        .expect("issue")
        .into_inner();
    assert_eq!(issue.author.as_ref().unwrap().id, agent);
    let by = issue.inspired_by.clone().expect("inspired_by");
    assert_eq!(by.post_id, post_id);
    assert_eq!(by.repo, rref(UPSTREAM));
    assert_eq!(by.change_number, cr.number);

    // 5. The post counts and lists the adoption.
    let post = get_post(Some(&dev), s.client.channel(), &post_id).await;
    assert_eq!(post.counts.as_ref().unwrap().adoptions, 1);
    let list = adoptions(&dev, &post_id).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].repo, rref(TARGET));
    assert_eq!(list[0].subject.as_ref().unwrap().id, issue.id);
    assert_eq!(
        list[0].subject.as_ref().unwrap().kind,
        v1::SubjectKind::Issue as i32
    );
    assert_eq!(list[0].title, issue.title);

    // 6. Outsiders: signed out, and an agent outside the org.
    let signed_out = get_post(None, s.client.channel(), &post_id).await;
    assert_eq!(signed_out.counts.as_ref().unwrap().adoptions, 0);
    let stranger = s.client.create_agent("stranger").await.expect("agent");
    let stranger_token = s
        .client
        .mint_token(&stranger, &[], 0, 0)
        .await
        .expect("token");
    let stranger = Client::new(s.client.channel().clone(), stranger_token, UPSTREAM);
    let seen = get_post(Some(&stranger), s.client.channel(), &post_id).await;
    assert_eq!(seen.counts.as_ref().unwrap().adoptions, 0);
    assert!(adoptions(&stranger, &post_id).await.is_empty());

    // Exactly once: later windows start nothing more.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(adopt_runs(&dev).await.len(), 1);
}
