//! Linearizability of ref CAS under concurrent writers (SPEC §14.7): exactly one winner per
//! version and no ref-log gaps.

use std::collections::BTreeSet;
use std::sync::Arc;

use bee_core::{ChangeId, ObjectId, Reason};
use bee_store::{Expected, LandedEntry, MetaStore, RefStep, RefUpdate};

use crate::fixtures::{Change, TestEnv, ref_name};

async fn drafts(env: &TestEnv, repo: &crate::TestRepo, n: usize) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut parent = None;
    for i in 0..n {
        let id = env
            .commit(repo, parent, &[Change::put("f", format!("{i}"))])
            .await
            .unwrap();
        out.push(id);
        parent = Some(id);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn n_way_cas_has_exactly_one_winner() {
    let env = Arc::new(TestEnv::new());
    let r = env.create_repo("acme", "app").await.unwrap();
    let targets = drafts(&env, &r, 33).await;
    let name = ref_name("ws/agent/race");
    env.meta
        .update_ref(RefUpdate::set(
            r.repo,
            name.clone(),
            Expected::Absent,
            Some(targets[0]),
            "p",
            1,
        ))
        .await
        .unwrap();
    for round in 0..5u64 {
        let version = round + 1;
        let mut tasks = Vec::new();
        for (i, t) in targets[1..].iter().enumerate() {
            let env = env.clone();
            let name = name.clone();
            let t = *t;
            tasks.push(tokio::spawn(async move {
                let res = env
                    .meta
                    .update_ref(RefUpdate::set(
                        r.repo,
                        name,
                        Expected::Version(version),
                        Some(t),
                        format!("writer-{i}"),
                        1,
                    ))
                    .await;
                (i, res)
            }));
        }
        let mut winners = Vec::new();
        for t in tasks {
            let (i, res) = t.await.unwrap();
            match res {
                Ok(up) => {
                    assert_eq!(up.new_version, version + 1);
                    winners.push(i);
                }
                Err(e) => assert_eq!(e.reason, Reason::VersionMismatch),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: winners {winners:?}");
    }
    let log = env.meta.ref_log(r.repo, &name, 1, 100).await.unwrap();
    let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
    assert_eq!(versions, (1..=6).collect::<Vec<_>>());
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_retrying_writers_never_lose_or_duplicate_updates() {
    const WRITERS: usize = 16;
    const EACH: usize = 12;
    let env = Arc::new(TestEnv::new());
    let r = env.create_repo("acme", "app").await.unwrap();
    let targets = drafts(&env, &r, WRITERS * EACH).await;
    let name = ref_name("main");
    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let env = env.clone();
        let name = name.clone();
        let mine: Vec<ObjectId> = targets[w * EACH..(w + 1) * EACH].to_vec();
        tasks.push(tokio::spawn(async move {
            let mut won = Vec::new();
            for (k, t) in mine.into_iter().enumerate() {
                // Each update also lands a unique ChangeId, so the landed index is exercised.
                let mut cid = [0u8; 16];
                cid[0] = w as u8 + 1;
                cid[1] = k as u8 + 1;
                loop {
                    let cur = env.meta.get_ref(r.repo, &name).await.unwrap();
                    let expected = match &cur {
                        Some(row) => Expected::Version(row.version),
                        None => Expected::Absent,
                    };
                    let res = env
                        .meta
                        .update_ref(RefUpdate {
                            repo: r.repo,
                            name: name.clone(),
                            expected,
                            steps: vec![RefStep {
                                target: Some(t),
                                landed: vec![LandedEntry {
                                    change_id: ChangeId::new(cid),
                                    source: t,
                                    result: t,
                                }],
                            }],
                            principal: format!("writer-{w}"),
                            at_ms: 1,
                            events: vec![],
                        })
                        .await;
                    match res {
                        Ok(up) => {
                            won.push(up.new_version);
                            break;
                        }
                        Err(e) => {
                            assert_eq!(e.reason, Reason::VersionMismatch);
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }
            won
        }));
    }
    let mut all: Vec<u64> = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    let total = WRITERS * EACH;
    let distinct: BTreeSet<u64> = all.iter().copied().collect();
    assert_eq!(distinct.len(), total, "a version was won twice");
    assert_eq!(distinct, (1..=total as u64).collect());
    let log = env
        .meta
        .ref_log(r.repo, &name, 1, usize::MAX)
        .await
        .unwrap();
    assert_eq!(log.len(), total);
    for (i, e) in log.iter().enumerate() {
        assert_eq!(e.version, i as u64 + 1);
        if i > 0 {
            assert_eq!(
                e.old,
                log[i - 1].new,
                "ref log chain broken at {}",
                e.version
            );
        }
    }
    assert_eq!(
        env.meta
            .list_landed(r.repo, Some(&name))
            .await
            .unwrap()
            .len(),
        total
    );
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
