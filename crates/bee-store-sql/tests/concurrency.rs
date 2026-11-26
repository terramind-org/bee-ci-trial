//! Linearizability of ref CAS under concurrent writers (SPEC §14.7), on SQLite and Postgres:
//! exactly one winner per version, no ref-log gaps, no lost or duplicated landed rows.

use crate::common;
use std::collections::BTreeSet;
use std::sync::Arc;

use bee_core::{ChangeId, ObjectId, Reason};
use bee_store::{Expected, LandedEntry, MetaStore, NewEvent, RefStep, RefUpdate};
use bee_testkit::fixtures::{Change, ref_name};
use common::{Env, Open, TestRepo};

async fn drafts<S: Open>(env: &Env<S>, repo: &TestRepo, n: usize) -> Vec<ObjectId> {
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

async fn n_way_cas_has_exactly_one_winner<S: Open>() {
    let env = Arc::new(env_or_skip!(S));
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
                Err(e) => assert_eq!(e.reason, Reason::VersionMismatch, "{e}"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: winners {winners:?}");
    }
    let log = env.meta.ref_log(r.repo, &name, 1, 100).await.unwrap();
    let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
    assert_eq!(versions, (1..=6).collect::<Vec<_>>());
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(n_way_cas_has_exactly_one_winner, multi);

async fn concurrent_creators_of_an_absent_ref_have_one_winner<S: Open>() {
    let env = Arc::new(env_or_skip!(S));
    let r = env.create_repo("acme", "app").await.unwrap();
    let targets = drafts(&env, &r, 12).await;
    let name = ref_name("ws/agent/new");
    let mut tasks = Vec::new();
    for t in targets {
        let env = env.clone();
        let name = name.clone();
        tasks.push(tokio::spawn(async move {
            env.meta
                .update_ref(RefUpdate::set(
                    r.repo,
                    name,
                    Expected::Absent,
                    Some(t),
                    "p",
                    1,
                ))
                .await
        }));
    }
    let mut wins = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(up) => {
                assert_eq!(up.new_version, 1);
                wins += 1;
            }
            Err(e) => assert_eq!(e.reason, Reason::VersionMismatch, "{e}"),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(
        env.meta.ref_log(r.repo, &name, 0, 10).await.unwrap().len(),
        1
    );
}
both!(concurrent_creators_of_an_absent_ref_have_one_winner, multi);

async fn concurrent_retrying_writers_never_lose_or_duplicate_updates<S: Open>() {
    const WRITERS: usize = 16;
    const EACH: usize = 12;
    let env = Arc::new(env_or_skip!(S));
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
                            events: vec![NewEvent {
                                kind: "ref.updated".into(),
                                payload: vec![w as u8, k as u8],
                                at_ms: 1,
                            }],
                        })
                        .await;
                    match res {
                        Ok(up) => {
                            assert_eq!(up.event_seqs.len(), 1);
                            won.push((up.new_version, up.event_seqs[0]));
                            break;
                        }
                        Err(e) => {
                            // Writers of one ref in one process take turns, so the only way
                            // to lose is the compare-and-swap: never the store's own
                            // serialization retries running out (`UNAVAILABLE`).
                            assert_eq!(e.reason, Reason::VersionMismatch, "{e}");
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }
            won
        }));
    }
    let mut all: Vec<(u64, u64)> = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    let total = WRITERS * EACH;
    let versions: BTreeSet<u64> = all.iter().map(|(v, _)| *v).collect();
    assert_eq!(versions.len(), total, "a version was won twice");
    assert_eq!(versions, (1..=total as u64).collect());
    let seqs: BTreeSet<u64> = all.iter().map(|(_, s)| *s).collect();
    assert_eq!(
        seqs,
        (1..=total as u64).collect(),
        "event numbers lost or reused"
    );
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
both!(
    concurrent_retrying_writers_never_lose_or_duplicate_updates,
    multi
);

async fn concurrent_uploads_of_the_same_objects_dedupe<S: Open>() {
    let env = Arc::new(env_or_skip!(S));
    let r = env.create_repo("acme", "app").await.unwrap();
    let mut tasks = Vec::new();
    for w in 0..8u8 {
        let env = env.clone();
        tasks.push(tokio::spawn(async move {
            // Every writer uploads the same 50 blobs plus 10 of its own.
            let mut objs: Vec<(ObjectId, Vec<u8>)> = (0..50u8)
                .map(|i| {
                    let b = vec![b's', i];
                    (bee_object::blob_id(&b), b)
                })
                .collect();
            for i in 0..10u8 {
                let b = vec![b'o', w, i];
                objs.push((bee_object::blob_id(&b), b));
            }
            env.writer
                .upload(
                    r.org,
                    objs,
                    Some(bee_store::UploadRecord {
                        repo: Some(r.repo),
                        principal: format!("w{w}"),
                    }),
                    1,
                )
                .await
                .unwrap()
        }));
    }
    let mut inserted = 0;
    for t in tasks {
        inserted += t
            .await
            .unwrap()
            .iter()
            .filter(|o| **o == bee_store::PutOutcome::Inserted)
            .count();
    }
    assert_eq!(inserted, 50 + 8 * 10);
    let rows = env.meta.list_objects(r.org, None, 1000).await.unwrap();
    assert_eq!(rows.len(), 130);
    let shared = bee_object::blob_id(&[b's', 0]);
    assert_eq!(env.meta.get_uploads(r.org, &shared).await.unwrap().len(), 8);
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(concurrent_uploads_of_the_same_objects_dedupe, multi);
