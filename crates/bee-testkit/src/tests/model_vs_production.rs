//! Property tests comparing production against the reference model on random histories.

use std::collections::BTreeMap;

use bee_core::{ChangeId, Mode, ObjectId, Reason, RefName, RepoPath};
use bee_object::{DeltaEntry, DeltaOp, MemoryTrees, apply_delta, blob_id, diff_trees};
use bee_store::{Expected, LandedEntry, MetaStore, RefStep, RefUpdate};
use proptest::prelude::*;

use crate::fixtures::{CommitSpec, TestEnv, TestRepo, ref_name};
use crate::model::Graph;
use crate::model::blame::blame;
use crate::model::refs::{LandOutcome, RefModel, RefModelError};
use crate::model::tree::{self as mtree, FileMap, Leaf};
use crate::rng::TestRng;

const NAMES: [&str; 5] = ["a", "b", "a.b", "c", "a-"];

fn random_path(rng: &mut TestRng) -> Vec<u8> {
    let depth = 1 + rng.below(3) as usize;
    let comps: Vec<&str> = (0..depth).map(|_| *rng.pick(&NAMES)).collect();
    comps.join("/").into_bytes()
}

fn random_leaf(rng: &mut TestRng) -> Leaf {
    match rng.below(10) {
        0 => Leaf {
            mode: Mode::Gitlink,
            id: ObjectId::git_foreign(&[rng.below(3) as u8; 20]).unwrap(),
            size: 0,
        },
        n => {
            let data = format!("v{}", rng.below(4));
            let mode = match n {
                1 => Mode::Executable,
                2 => Mode::Symlink,
                _ => Mode::File,
            };
            Leaf {
                mode,
                id: blob_id(data.as_bytes()),
                size: data.len() as u64,
            }
        }
    }
}

fn inside(dir: &[u8], p: &[u8]) -> bool {
    p.len() > dir.len() && p.starts_with(dir) && p[dir.len()] == b'/'
}

/// A random valid next snapshot: puts (replacing files above and directories below) and
/// deletes (of a file or a whole directory).
fn mutate(rng: &mut TestRng, files: &FileMap) -> FileMap {
    let mut out = files.clone();
    for _ in 0..1 + rng.below(4) {
        if !out.is_empty() && rng.chance(1, 3) {
            let keys: Vec<Vec<u8>> = out.keys().cloned().collect();
            let k = rng.pick(&keys).clone();
            let cut = if rng.chance(1, 2) {
                k.iter().position(|b| *b == b'/').map(|i| k[..i].to_vec())
            } else {
                None
            };
            match cut {
                Some(dir) => out.retain(|p, _| !inside(&dir, p)),
                None => {
                    out.remove(&k);
                }
            }
        } else {
            let p = random_path(rng);
            for (i, b) in p.iter().enumerate() {
                if *b == b'/' {
                    out.remove(&p[..i]);
                }
            }
            out.retain(|k, _| !inside(&p, k));
            out.insert(p, random_leaf(rng));
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Valid histories: production's apply_delta builds the model's root, production's diff
    /// equals the model's diff (invariant D), and the model applies its own diff.
    #[test]
    fn apply_delta_and_diff_match_the_model(seed in any::<u64>(), steps in 1usize..14) {
        let mut rng = TestRng::seed(seed);
        let mut files = FileMap::new();
        let mut source = MemoryTrees::new();
        let (mut root, trees) = mtree::trees(&files);
        source.extend(trees);
        for _ in 0..steps {
            let next = mutate(&mut rng, &files);
            let delta = mtree::diff(&files, &next);
            prop_assert_eq!(mtree::apply(&files, &delta), Ok(next.clone()));
            let built = apply_delta(&source, Some(&root), &delta);
            prop_assert!(built.is_ok(), "production rejected a valid delta: {:?}", built);
            let built = built.unwrap();
            prop_assert_eq!(built.root, mtree::root_id(&next));
            source.extend(built.trees);
            let prod = diff_trees(&source, Some(&root), Some(&built.root)).unwrap();
            prop_assert_eq!(prod, delta);
            root = built.root;
            files = next;
        }
    }

    /// Arbitrary (often invalid) deltas: production and the model accept exactly the same
    /// ones, and agree on the result when they accept.
    #[test]
    fn invalid_deltas_are_rejected_by_both(seed in any::<u64>()) {
        let mut rng = TestRng::seed(seed);
        let mut files = FileMap::new();
        for _ in 0..rng.below(4) {
            files = mutate(&mut rng, &files);
        }
        let next = mutate(&mut rng, &files);
        let mut entries: BTreeMap<Vec<u8>, DeltaOp> = mtree::diff(&files, &next)
            .into_iter()
            .map(|e| (e.path.as_bytes().to_vec(), e.op))
            .collect();
        for _ in 0..1 + rng.below(2) {
            match rng.below(5) {
                0 if !entries.is_empty() => {
                    let keys: Vec<Vec<u8>> = entries.keys().cloned().collect();
                    entries.remove(rng.pick(&keys));
                }
                1 if !files.is_empty() => {
                    let keys: Vec<Vec<u8>> = files.keys().cloned().collect();
                    let k = rng.pick(&keys).clone();
                    let l = files[&k];
                    entries.insert(k, DeltaOp::Put { mode: l.mode, id: l.id, size: l.size });
                }
                2 => {
                    entries.insert(random_path(&mut rng), DeltaOp::Delete);
                }
                3 => {
                    let l = random_leaf(&mut rng);
                    entries.insert(random_path(&mut rng), DeltaOp::Put { mode: l.mode, id: l.id, size: l.size });
                }
                _ => {
                    if let Some(k) = files.keys().next().cloned()
                        && let Some(i) = k.iter().position(|b| *b == b'/')
                    {
                        entries.insert(k[..i].to_vec(), DeltaOp::Delete);
                    }
                }
            }
        }
        let delta: Vec<DeltaEntry> = entries
            .into_iter()
            .map(|(p, op)| DeltaEntry { path: RepoPath::new(p).unwrap(), op })
            .collect();
        let (root, trees) = mtree::trees(&files);
        let mut source = MemoryTrees::new();
        source.extend(trees);
        let model = mtree::apply(&files, &delta);
        let prod = apply_delta(&source, Some(&root), &delta);
        prop_assert_eq!(model.is_ok(), prod.is_ok(), "model {:?} vs production {:?}", model, prod);
        if let (Ok(m), Ok(p)) = (model, prod) {
            prop_assert_eq!(mtree::root_id(&m), p.root);
        }
    }

    /// Blame: with unique line contents the true origin of each line is known; the model
    /// recovers it.
    #[test]
    fn blame_recovers_line_origins(seed in any::<u64>(), steps in 1usize..20) {
        let mut rng = TestRng::seed(seed);
        let mut lines: Vec<(usize, String)> = Vec::new();
        let mut history = Vec::new();
        let mut counter = 0;
        for commit in 0..steps {
            for _ in 0..1 + rng.below(3) {
                counter += 1;
                match rng.below(3) {
                    0 if !lines.is_empty() => {
                        let i = rng.index(lines.len());
                        lines.remove(i);
                    }
                    1 if !lines.is_empty() => {
                        let i = rng.index(lines.len());
                        lines[i] = (commit, format!("line {counter}\n"));
                    }
                    _ => {
                        let i = rng.index(lines.len() + 1);
                        lines.insert(i, (commit, format!("line {counter}\n")));
                    }
                }
            }
            let content: String = lines.iter().map(|(_, l)| l.as_str()).collect();
            history.push((commit, Some(content.into_bytes())));
        }
        let got: Vec<usize> = blame(&history).into_iter().map(|(c, _)| c).collect();
        let want: Vec<usize> = lines.iter().map(|(c, _)| *c).collect();
        prop_assert_eq!(got, want);
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random DAGs written through the store: parent edges, children and generation rows
    /// agree with the model graph, and the model's merge bases are well formed.
    #[test]
    fn commit_graph_matches_the_model(seed in any::<u64>(), n in 2usize..16) {
        let mut rng = TestRng::seed(seed);
        let env = TestEnv::with_seed(seed);
        rt().block_on(async {
            let r = env.create_repo("acme", "app").await.unwrap();
            let mut graph = Graph::new();
            let mut ids: Vec<ObjectId> = Vec::new();
            for i in 0..n {
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
                    .commit_with(&r, CommitSpec {
                        parents: parents.clone(),
                        changes: vec![crate::Change::put(&format!("f{i}"), format!("{i}"))],
                        message: format!("c{i}"),
                        change_id: None,
                    })
                    .await
                    .unwrap();
                graph.add(id, parents);
                ids.push(id);
            }
            for id in &ids {
                assert_eq!(env.meta.parents(r.repo, id).await.unwrap(), graph.parents(id));
                assert_eq!(env.meta.children(r.repo, id).await.unwrap(), graph.children(id));
                let g = env
                    .meta
                    .get_derived(r.repo, id, "graph", 1, bee_store::ReadFrom::Primary)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(u64::from_be_bytes(g.try_into().unwrap()), graph.generation(id));
            }
            let a = *rng.pick(&ids);
            let b = *rng.pick(&ids);
            let bases = graph.merge_bases(&a, &b);
            for x in &bases {
                assert!(graph.is_ancestor(x, &a) && graph.is_ancestor(x, &b));
                for y in &bases {
                    assert!(x == y || !graph.is_ancestor(x, y));
                }
            }
            assert!(env.fsck(r.org).await.unwrap().is_clean());
        });
    }

    /// Random ref updates and lands against the in-memory store and the model: same
    /// outcomes, same refs, same ref logs, same landed index.
    #[test]
    fn refs_and_landing_match_the_model(seed in any::<u64>(), ops in 1usize..40) {
        let mut rng = TestRng::seed(seed);
        let env = TestEnv::with_seed(seed);
        rt().block_on(async {
            let r = env.create_repo("acme", "app").await.unwrap();
            let pool = drafts(&env, &r, 6).await;
            let names: Vec<RefName> = ["main", "ws/a/x", "green/ci"].iter().map(|n| ref_name(n)).collect();
            let cids: Vec<ChangeId> = (1..=6u8).map(|i| ChangeId::new([i; 16])).collect();
            let mut model = RefModel::new();
            for _ in 0..ops {
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
                        let target = if rng.chance(1, 8) { None } else { Some(*rng.pick(&pool)) };
                        let landed = if rng.chance(1, 3) {
                            let t = target.unwrap_or(pool[0]);
                            vec![LandedEntry { change_id: *rng.pick(&cids), source: t, result: t }]
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
        });
    }
}

async fn drafts(env: &TestEnv, r: &TestRepo, n: usize) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut parent = None;
    for i in 0..n {
        let id = env
            .commit(r, parent, &[crate::Change::put("f", format!("{i}"))])
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

/// Land through the store the way SPEC §7.4 describes, and through the model; compare.
async fn land_both(
    env: &TestEnv,
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
    // The "rebase" oracle: results are existing changesets chosen from the source.
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

async fn same_state(env: &TestEnv, r: &TestRepo, model: &RefModel, names: &[RefName]) {
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
