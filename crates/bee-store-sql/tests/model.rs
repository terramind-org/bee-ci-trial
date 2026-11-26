//! Random histories through the SQL stores compared with the testkit's reference models
//! (SPEC §14.1): ref updates and lands against `RefModel`, commit graphs against `Graph`.
//! Seeds are fixed so a failure reproduces.

use crate::common;
use bee_core::{ChangeId, ObjectId, Reason, RefName};
use bee_store::{Expected, LandedEntry, MetaStore, ReadFrom, RefStep, RefUpdate};
use bee_testkit::TestRng;
use bee_testkit::fixtures::{Change, CommitSpec, ref_name};
use bee_testkit::model::{Graph, LandOutcome, RefModel, RefModelError};
use common::{Env, Open, TestRepo};

async fn drafts<S: Open>(env: &Env<S>, r: &TestRepo, n: usize) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut parent = None;
    for i in 0..n {
        let id = env
            .commit(r, parent, &[Change::put("f", format!("{i}"))])
            .await
            .unwrap();
        out.push(id);
        parent = Some(id);
    }
    out
}

fn compare(want: &Result<(u64, u64), RefModelError>, got: &bee_core::Result<(u64, u64)>) {
    match (want, got) {
        (Ok(w), Ok(g)) => assert_eq!(w, g),
        (Err(RefModelError::VersionMismatch { current }), Err(e)) => {
            assert_eq!(e.reason, Reason::VersionMismatch);
            assert_eq!(
                e.payload.get("current_version"),
                Some(&bee_core::PayloadValue::Int(*current as i64))
            );
        }
        (Err(RefModelError::AlreadyLanded { change_id }), Err(e)) => {
            assert_eq!(e.reason, Reason::AlreadyLanded);
            assert_eq!(
                e.payload.get("change_id"),
                Some(&bee_core::PayloadValue::Str(change_id.to_text()))
            );
        }
        (Err(RefModelError::Invalid), Err(e)) => assert_eq!(e.reason, Reason::InvalidArgument),
        (w, g) => panic!("model {w:?} vs store {g:?}"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn land_both<S: Open>(
    env: &Env<S>,
    r: &TestRepo,
    model: &mut RefModel,
    rng: &mut TestRng,
    name: &RefName,
    pool: &[ObjectId],
    cids: &[ChangeId],
) {
    let stack: Vec<(ChangeId, ObjectId)> = (0..1 + rng.below(3))
        .map(|_| (*rng.pick(cids), *rng.pick(pool)))
        .collect();
    let mut rebase = |_tip: Option<ObjectId>, src: ObjectId| src;
    let want = model.land(name, &stack, &mut rebase);
    let cur = env.meta.get_ref(r.repo, name).await.unwrap();
    let (tip, version) = cur.map(|c| (c.target, c.version)).unwrap_or((None, 0));
    let ids: Vec<ChangeId> = stack.iter().map(|(c, _)| *c).collect();
    let rows = env.meta.get_landed(r.repo, name, &ids).await.unwrap();
    let mut results = Vec::new();
    let mut entries = Vec::new();
    let mut running = tip;
    let mut store_err = None;
    for ((cid, src), row) in stack.iter().zip(rows) {
        match row {
            Some(row) if row.source == *src => results.push(row.result),
            Some(_) => {
                store_err = Some(RefModelError::AlreadyLanded { change_id: *cid });
                break;
            }
            None => {
                let res = rebase(running, *src);
                running = Some(res);
                results.push(res);
                entries.push(LandedEntry {
                    change_id: *cid,
                    source: *src,
                    result: res,
                });
            }
        }
    }
    let got: Result<LandOutcome, RefModelError> = match store_err {
        Some(e) => Err(e),
        None if entries.is_empty() => Ok(LandOutcome::AlreadyRecorded { results }),
        None => {
            let expected = if tip.is_none() {
                Expected::Absent
            } else {
                Expected::Version(version)
            };
            match env
                .meta
                .update_ref(RefUpdate {
                    repo: r.repo,
                    name: name.clone(),
                    expected,
                    steps: vec![RefStep {
                        target: running,
                        landed: entries,
                    }],
                    principal: "lander".into(),
                    at_ms: 1,
                    events: vec![],
                })
                .await
            {
                Ok(up) => Ok(LandOutcome::Landed {
                    version: up.new_version,
                    results,
                }),
                Err(e) if e.reason == Reason::AlreadyLanded => Err(RefModelError::AlreadyLanded {
                    change_id: ChangeId::parse(match e.payload.get("change_id") {
                        Some(bee_core::PayloadValue::Str(s)) => s,
                        _ => panic!("no change_id"),
                    })
                    .unwrap(),
                }),
                Err(e) => panic!("unexpected {e}"),
            }
        }
    };
    assert_eq!(want, got);
}

async fn same_state<S: Open>(env: &Env<S>, r: &TestRepo, model: &RefModel, names: &[RefName]) {
    let store_refs: Vec<_> = env
        .meta
        .list_refs(r.repo, "")
        .await
        .unwrap()
        .into_iter()
        .map(|x| (x.name, x.target, x.version))
        .collect();
    assert_eq!(store_refs, model.refs());
    for n in names {
        let log = env.meta.ref_log(r.repo, n, 0, usize::MAX).await.unwrap();
        let got: Vec<_> = log
            .iter()
            .map(|e| (e.version, e.old, e.new, e.landed.clone()))
            .collect();
        let want: Vec<_> = model
            .log(n)
            .iter()
            .map(|e| (e.version, e.old, e.new, e.landed.clone()))
            .collect();
        assert_eq!(got, want);
    }
    let landed: Vec<_> = env
        .meta
        .list_landed(r.repo, None)
        .await
        .unwrap()
        .into_iter()
        .map(|l| (l.ref_name, l.change_id, l.source, l.result, l.version))
        .collect();
    assert_eq!(landed, model.landed_all());
}

async fn refs_and_landing_match_the_model<S: Open>() {
    for seed in 1..=10u64 {
        let mut rng = TestRng::seed(seed);
        let env = env_or_skip!(S, seed);
        let r = env.create_repo("acme", "app").await.unwrap();
        let pool = drafts(&env, &r, 6).await;
        let names: Vec<RefName> = ["main", "ws/a/x", "green/ci"]
            .iter()
            .map(|n| ref_name(n))
            .collect();
        let cids: Vec<ChangeId> = (1..=6u8).map(|i| ChangeId::new([i; 16])).collect();
        let mut model = RefModel::new();
        for _ in 0..40 {
            let name = rng.pick(&names).clone();
            if rng.chance(1, 3) {
                land_both(&env, &r, &mut model, &mut rng, &name, &pool, &cids).await;
                continue;
            }
            let current = model.get(&name);
            let expected = match rng.below(4) {
                0 => Expected::Absent,
                1 => Expected::Version(rng.below(4)),
                _ => match current {
                    Some((_, v)) => Expected::Version(v),
                    None => Expected::Absent,
                },
            };
            let steps: Vec<RefStep> = (0..rng.below(3) + u64::from(rng.chance(9, 10)))
                .map(|_| {
                    let target = if rng.chance(1, 8) {
                        None
                    } else {
                        Some(*rng.pick(&pool))
                    };
                    let landed = if rng.chance(1, 3) {
                        let t = target.unwrap_or(pool[0]);
                        vec![LandedEntry {
                            change_id: *rng.pick(&cids),
                            source: t,
                            result: t,
                        }]
                    } else {
                        vec![]
                    };
                    RefStep { target, landed }
                })
                .collect();
            let model_steps: Vec<_> = steps.iter().map(|s| (s.target, s.landed.clone())).collect();
            let want = model.update(&name, expected, &model_steps);
            let got = env
                .meta
                .update_ref(RefUpdate {
                    repo: r.repo,
                    name: name.clone(),
                    expected,
                    steps,
                    principal: "p".into(),
                    at_ms: 1,
                    events: vec![],
                })
                .await;
            compare(&want, &got.map(|u| (u.old_version, u.new_version)));
        }
        same_state(&env, &r, &model, &names).await;
        assert!(env.fsck(r.org).await.unwrap().is_clean());
    }
}
both!(refs_and_landing_match_the_model);

async fn commit_graph_matches_the_model<S: Open>() {
    for seed in 1..=6u64 {
        let mut rng = TestRng::seed(seed);
        let env = env_or_skip!(S, seed);
        let r = env.create_repo("acme", "app").await.unwrap();
        let mut graph = Graph::new();
        let mut ids: Vec<ObjectId> = Vec::new();
        for i in 0..14 {
            let mut parents = Vec::new();
            if !ids.is_empty() && rng.chance(9, 10) {
                parents.push(*rng.pick(&ids));
                if ids.len() > 1 && rng.chance(1, 3) {
                    let second = *rng.pick(&ids);
                    if second != parents[0] {
                        parents.push(second);
                    }
                }
            }
            let id = env
                .commit_with(
                    &r,
                    CommitSpec {
                        parents: parents.clone(),
                        changes: vec![Change::put(&format!("f{i}"), format!("{i}"))],
                        message: format!("c{i}"),
                        change_id: None,
                    },
                )
                .await
                .unwrap();
            graph.add(id, parents);
            ids.push(id);
        }
        for id in &ids {
            assert_eq!(
                env.meta.parents(r.repo, id).await.unwrap(),
                graph.parents(id)
            );
            assert_eq!(
                env.meta.children(r.repo, id).await.unwrap(),
                graph.children(id)
            );
            let g = env
                .meta
                .get_derived(r.repo, id, "graph", 1, ReadFrom::Primary)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                u64::from_be_bytes(g.try_into().unwrap()),
                graph.generation(id)
            );
        }
        assert!(env.fsck(r.org).await.unwrap().is_clean());
    }
}
both!(commit_graph_matches_the_model);
