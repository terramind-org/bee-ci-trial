//! Many-agent simulation against one in-process server (SPEC §14.10, §15.4, PRODUCT §4.25).
//!
//! N agents (100 in the gate, 1,000 under `BEE_SCALE_FULL=1`), each registered by the dev
//! human and holding its own delegated token with a budget, run realistic loops: sparse
//! clone of their directory, lease it, commit an edit, open a change request, get it reviewed
//! by another agent, land it (through the change request or directly), release the lease.
//! Event subscribers follow the repo while they work, and a bulk flood (fsck runs) competes
//! for capacity. Afterwards the test asserts:
//!
//! - no lost updates: every agent's last edit is on `main`, the ref log is gap-free and the
//!   landed index holds exactly the lands that were acknowledged;
//! - no duplicate lands: every ChangeId landed once;
//! - every land and every state change by an agent is audit-traceable to the human at the root
//!   of its delegation chain (audit log and `ref.updated` events);
//! - budgets: an agent past its changeset budget gets `BUDGET_EXHAUSTED`, and a separate run
//!   shows agents under one human sharing the delegation root's quota (`QUOTA_EXCEEDED`);
//! - fairness: under a bulk flood the bulk tier is shed while no interactive or land request
//!   of any agent is refused;
//! - subscribers saw every version of `main`;
//! - exactly one of many agents racing for one exclusive lease gets it.

use crate::scale_support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bee_conformance::load::client::{Client, RpcError, change_id, ref_updated};
use bee_core::{ObjectId, RefName};
use bee_proto::v1;
use scale_support::*;

struct Agent {
    id: String,
    client: Client,
}

/// Registers `n` agents under the dev human, each with a token limited to its own directory
/// and a budget of `max_changesets` changesets.
async fn register(
    s: &Srv,
    channels: &[tonic::transport::Channel],
    n: usize,
    max_changesets: u64,
) -> Vec<Agent> {
    let mut handles = Vec::new();
    for i in 0..n {
        let root = s.client.clone();
        let ch = channels[i % channels.len()].clone();
        handles.push(tokio::spawn(async move {
            let id = root
                .create_agent(&format!("agent-{i}"))
                .await
                .expect("create agent");
            root.add_member("dev", &id).await.expect("org member");
            root.add_writer(&id).await.expect("repo writer");
            let token = root
                .mint_token(
                    &id,
                    &[&format!("/agents/{i}"), "/shared"],
                    max_changesets,
                    0,
                )
                .await
                .expect("mint token");
            Agent {
                id,
                client: Client::new(ch, token, "dev/playground"),
            }
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.expect("register task"));
    }
    out
}

#[derive(Default)]
struct Tally {
    lands: AtomicU64,
    via_change_request: AtomicU64,
    reviews: AtomicU64,
    leases: AtomicU64,
    refused: AtomicU64,
}

fn refused(e: &RpcError) -> bool {
    e.reason() == "RESOURCE_EXHAUSTED"
}

/// One agent's loop over `rounds` iterations; returns the content of its last edit.
async fn agent_loop(
    i: usize,
    me: Arc<Agent>,
    reviewer: Arc<Agent>,
    rounds: usize,
    tally: Arc<Tally>,
) -> Vec<u8> {
    let c = &me.client;
    let dir = format!("agents/{i}");
    let mut last = Vec::new();
    for r in 0..rounds {
        let step = |what: &str, e: RpcError| -> ! {
            if refused(&e) {
                tally.refused.fetch_add(1, Ordering::Relaxed);
            }
            panic!("agent {i} round {r}: {what}: {e}")
        };
        // Sparse clone: the agent's directory only.
        let files = c
            .list_tree("main", &dir)
            .await
            .unwrap_or_else(|e| step("list tree", e));
        let ids: Vec<ObjectId> = files.iter().map(|f| f.1).collect();
        c.get_objects(&ids)
            .await
            .unwrap_or_else(|e| step("fetch", e));
        let lease = c
            .acquire_lease("main", &[dir.as_str()])
            .await
            .unwrap_or_else(|e| step("lease", e));
        tally.leases.fetch_add(1, Ordering::Relaxed);
        let (tip, _) = c
            .resolve("main")
            .await
            .unwrap_or_else(|e| step("resolve", e))
            .expect("main");
        last = format!("agent {i} round {r}\n").into_bytes();
        let head = c
            .commit(
                &tip,
                &[(format!("{dir}/work.txt"), last.clone())],
                change_id(((i as u64) << 20) | r as u64 | (1 << 50)),
                &format!("agent {i} round {r}"),
            )
            .await
            .unwrap_or_else(|e| step("commit", e));
        let cr = c
            .open_change("main", &head, &format!("agent {i} round {r}"))
            .await
            .unwrap_or_else(|e| step("open change", e));
        reviewer
            .client
            .approve(&cr, 1)
            .await
            .unwrap_or_else(|e| step("review", e));
        tally.reviews.fetch_add(1, Ordering::Relaxed);
        let landed = if (i + r).is_multiple_of(2) {
            tally.via_change_request.fetch_add(1, Ordering::Relaxed);
            c.land_change(&cr).await
        } else {
            c.land("main", &head).await
        }
        .unwrap_or_else(|e| step("land", e));
        assert_eq!(landed.members.len(), 1);
        assert!(!landed.members[0].skipped);
        tally.lands.fetch_add(1, Ordering::Relaxed);
        c.release_lease(&lease)
            .await
            .unwrap_or_else(|e| step("release", e));
    }
    last
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn many_agents_work_without_lost_updates_and_traceably() {
    let n = size(100, 1_000);
    let rounds = size(2, 3);
    let (s, _dir) = start_fresh(
        |c| {
            // Bulk work may hold two slots; interactive and land keep the rest.
            c.admission.max_concurrent = 4096;
            c.admission.reserved_interactive_land = 3072;
            c.admission.max_bulk = 2;
            c.admission.queue_wait_ms = 100;
        },
        None,
    )
    .await;
    let human = s.user();
    let mut channels = Vec::new();
    for _ in 0..16 {
        channels.push(s.channel().await);
    }
    // Seed one file per agent directory so every sparse clone has something to fetch.
    let (tip, _) = main_tip(&s.client).await;
    let seeds: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| {
            (
                format!("agents/{i}/seed.txt"),
                format!("seed {i}\n").into_bytes(),
            )
        })
        .collect();
    let seeded = s
        .client
        .commit(&tip, &seeds, change_id(1), "seed")
        .await
        .expect("seed");
    s.client.land("main", &seeded).await.expect("land seed");
    let (_, start_version) = main_tip(&s.client).await;

    let agents: Vec<Arc<Agent>> = register(&s, &channels, n, rounds as u64)
        .await
        .into_iter()
        .map(Arc::new)
        .collect();

    // Exactly one of the agents racing for one exclusive lease gets it.
    let mut race = Vec::new();
    for a in agents.iter().take(20) {
        let a = a.clone();
        race.push(tokio::spawn(async move {
            a.client.acquire_lease("main", &["shared/config"]).await
        }));
    }
    let mut won = Vec::new();
    let mut lost = Vec::new();
    for h in race {
        match h.await.expect("race task") {
            Ok(id) => won.push(id),
            Err(e) => lost.push(e.to_string()),
        }
    }
    assert_eq!(
        won.len(),
        1,
        "one exclusive lease holder; refusals: {lost:?}"
    );

    // Subscribers follow main while the agents work.
    let mut subs = Vec::new();
    for a in agents.iter().take(5) {
        let mut stream = a
            .client
            .subscribe(&["ref.updated"], 0)
            .await
            .expect("subscribe");
        let want = start_version + (n * rounds) as u64;
        subs.push(tokio::spawn(async move {
            let mut seen = BTreeSet::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
            while !(start_version + 1..=want).all(|v| seen.contains(&v)) {
                match tokio::time::timeout_at(deadline, stream.message()).await {
                    Ok(Ok(Some(e))) => {
                        if let Some((name, _, v)) = ref_updated(&e)
                            && name == "main"
                        {
                            seen.insert(v);
                        }
                    }
                    other => panic!("subscriber stopped: {other:?}"),
                }
            }
            seen.len()
        }));
    }

    // A bulk flood competes for capacity the whole time.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shed = Arc::new(AtomicU64::new(0));
    let mut flood = Vec::new();
    for k in 0..8 {
        let c = s.client.clone();
        let (stop, shed) = (stop.clone(), shed.clone());
        flood.push(tokio::spawn(async move {
            let mut admin = v1::admin_service_client::AdminServiceClient::new(c.channel().clone());
            let mut done = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let r = admin
                    .run_fsck(c.req(v1::RunFsckRequest {
                        org: "dev".into(),
                        ..Default::default()
                    }))
                    .await;
                match r {
                    Ok(_) => done += 1,
                    Err(st) => {
                        let e = RpcError::from(st);
                        assert_eq!(e.reason(), "RESOURCE_EXHAUSTED", "flood {k}: {e}");
                        shed.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            done
        }));
    }

    let tally = Arc::new(Tally::default());
    let started = Instant::now();
    let mut work = Vec::new();
    for i in 0..n {
        let me = agents[i].clone();
        let reviewer = agents[(i + 1) % n].clone();
        work.push(tokio::spawn(agent_loop(
            i,
            me,
            reviewer,
            rounds,
            tally.clone(),
        )));
    }
    let mut last = Vec::new();
    for h in work {
        last.push(h.await.expect("agent task"));
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let mut fsck_runs = 0;
    for h in flood {
        fsck_runs += h.await.expect("flood task");
    }
    let lands = tally.lands.load(Ordering::Relaxed);
    eprintln!(
        "many agents: {n} agents x {rounds} rounds in {:.1}s: {lands} lands ({} via change requests), {} reviews, {} leases; bulk flood: {fsck_runs} fsck runs, {} shed",
        elapsed.as_secs_f64(),
        tally.via_change_request.load(Ordering::Relaxed),
        tally.reviews.load(Ordering::Relaxed),
        tally.leases.load(Ordering::Relaxed),
        shed.load(Ordering::Relaxed),
    );
    assert_eq!(lands, (n * rounds) as u64);
    // Fairness (SPEC §15.4): the bulk tier was shed, no agent request was refused.
    assert_eq!(tally.refused.load(Ordering::Relaxed), 0);
    assert!(
        shed.load(Ordering::Relaxed) > 0,
        "the bulk flood hit its tier's limit"
    );

    // No lost updates: every agent's last edit is on main.
    let (tip, final_version) = main_tip(&s.client).await;
    assert_eq!(final_version, start_version + lands);
    let tree: BTreeMap<String, ObjectId> = s
        .client
        .list_tree(&tip.to_text(), "agents")
        .await
        .expect("list")
        .into_iter()
        .map(|(p, id, _)| (p, id))
        .collect();
    for (i, content) in last.iter().enumerate() {
        let path = format!("agents/{i}/work.txt");
        assert_eq!(
            tree.get(&path),
            Some(&bee_object::blob_id(content)),
            "{path} holds the agent's last edit"
        );
    }
    s.assert_ref_log_continuous("main").await;
    // No duplicate lands.
    let main = RefName::new("main").expect("ref");
    let landed = s
        .srv
        .state
        .meta
        .list_landed(s.repo(), Some(&main))
        .await
        .expect("landed");
    let ids: BTreeSet<_> = landed.iter().map(|l| l.change_id).collect();
    assert_eq!(ids.len(), landed.len());
    assert_eq!(
        landed.len() as u64,
        lands + 1,
        "the agents' lands plus the seed"
    );

    // Audit: every action by an agent names the human at the root of its chain.
    let agent_ids: BTreeSet<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    let audit = s.srv.state.audit.read(1, 10_000_000).await.expect("audit");
    let mut agent_lands = 0;
    for e in &audit {
        if e.chain
            .first()
            .is_some_and(|p| agent_ids.contains(p.as_str()))
        {
            assert_eq!(
                e.chain.last().map(String::as_str),
                Some(human.as_str()),
                "audit entry traces to the human: {e:?}"
            );
            if e.action.ends_with("/Land") || e.action.ends_with("/LandChange") {
                agent_lands += 1;
            }
        }
    }
    assert!(
        agent_lands >= lands,
        "every land is audited ({agent_lands} < {lands})"
    );
    // Every ref.updated event names a principal chain ending at the human.
    let events = s
        .srv
        .state
        .meta
        .read_events(s.repo(), 0, 10_000_000, bee_store::ReadFrom::Primary)
        .await
        .expect("events");
    let mut traced = 0;
    for e in events.iter().filter(|e| e.kind == "ref.updated") {
        let v: serde_json::Value = serde_json::from_slice(&e.payload).expect("event json");
        if v["ref"] != "main" {
            continue;
        }
        let chain = v["principal_chain"].as_array().expect("chain");
        assert_eq!(
            chain.last().and_then(|c| c.as_str()),
            Some(human.as_str()),
            "{v}"
        );
        traced += 1;
    }
    assert!(traced as u64 >= lands);

    // Subscribers saw every version.
    for h in subs {
        let seen = h.await.expect("subscriber");
        assert!(seen as u64 >= lands);
    }

    // Budgets: one more commit is past every agent's changeset budget.
    let a = &agents[0];
    let (tip, _) = main_tip(&a.client).await;
    let e = a
        .client
        .commit(
            &tip,
            &[("agents/0/over.txt".into(), b"over\n".to_vec())],
            change_id(77),
            "over budget",
        )
        .await
        .expect_err("over budget");
    assert_eq!(e.reason(), "BUDGET_EXHAUSTED", "{e}");
    let (clean, v) = s.fsck_clean().await;
    assert!(clean, "fsck: {v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_under_one_human_share_the_delegation_root_quota() {
    let quota = 10u64;
    let (s, _dir) = start_fresh(|c| c.admission.root_max_changesets = quota, None).await;
    let channels = vec![s.channel().await];
    let agents = register(&s, &channels, 4, 0).await;
    let (tip, _) = main_tip(&s.client).await;
    let ok = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for (i, a) in agents.into_iter().enumerate() {
        let ok = ok.clone();
        handles.push(tokio::spawn(async move {
            let mut reasons = Vec::new();
            for k in 0..5u64 {
                let r = a
                    .client
                    .commit(
                        &tip,
                        &[(
                            format!("agents/{i}/q{k}.txt"),
                            format!("{i} {k}\n").into_bytes(),
                        )],
                        change_id(1_000 + (i as u64) * 10 + k),
                        "quota",
                    )
                    .await;
                match r {
                    Ok(_) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => reasons.push(e.reason()),
                }
            }
            reasons
        }));
    }
    let mut refused = Vec::new();
    for h in handles {
        refused.extend(h.await.expect("agent"));
    }
    // Four agents asked for 20 changesets; their human's root allows 10 in total.
    assert_eq!(ok.load(Ordering::Relaxed), quota, "{refused:?}");
    assert_eq!(refused.len(), 10);
    assert!(refused.iter().all(|r| r == "QUOTA_EXCEEDED"), "{refused:?}");

    // A land budget is checked before landing: one land allowed, the second refused.
    let id = s.client.create_agent("lander").await.expect("agent");
    let token = s.client.mint_token(&id, &[], 0, 1).await.expect("mint");
    let lander = s.client.with_token(token);
    let (tip, _) = main_tip(&lander).await;
    let a = lander
        .commit(
            &tip,
            &[("l/a.txt".into(), b"a\n".to_vec())],
            change_id(5_001),
            "a",
        )
        .await;
    // The root's changeset quota is used up, so this commit is refused too.
    assert_eq!(a.expect_err("root quota").reason(), "QUOTA_EXCEEDED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_land_budget_is_enforced_before_landing() {
    let (s, _dir) = start_fresh(|_| {}, None).await;
    let id = s.client.create_agent("lander").await.expect("agent");
    let token = s.client.mint_token(&id, &[], 0, 1).await.expect("mint");
    let lander = s.client.with_token(token);
    let (tip, _) = main_tip(&lander).await;
    let a = lander
        .commit(
            &tip,
            &[("l/a.txt".into(), b"a\n".to_vec())],
            change_id(5_001),
            "a",
        )
        .await
        .expect("a");
    let b = lander
        .commit(
            &tip,
            &[("l/b.txt".into(), b"b\n".to_vec())],
            change_id(5_002),
            "b",
        )
        .await
        .expect("b");
    lander
        .land("main", &a)
        .await
        .expect("first land within budget");
    let e = lander
        .land("main", &b)
        .await
        .expect_err("second land over budget");
    assert_eq!(e.reason(), "BUDGET_EXHAUSTED", "{e}");
    let (_, chain) = lander.who_am_i().await.expect("whoami");
    assert_eq!(
        chain.last(),
        Some(&s.user()),
        "the agent acts on behalf of the human"
    );
}
