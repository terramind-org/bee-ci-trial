//! Graph queries against the testkit reference model, and their complexity gates.

use crate::common;
use std::collections::BTreeSet;

use bee_core::ObjectId;
use bee_store::{Table, Writer};
use bee_testkit::model::Graph;
use bee_testkit::{CommitSpec, TestRng};
use common::{Env, put};
use proptest::prelude::*;

/// Builds a random DAG of `n` changesets (merges and extra roots included); returns ids in
/// creation order and the model.
async fn random_dag(env: &Env, seed: u64, n: usize) -> (Vec<ObjectId>, Graph) {
    let mut rng = TestRng::seed(seed);
    let mut ids: Vec<ObjectId> = Vec::new();
    let mut model = Graph::new();
    for i in 0..n {
        let parents: Vec<ObjectId> = if ids.is_empty() || rng.chance(1, 20) {
            Vec::new()
        } else if ids.len() >= 2 && rng.chance(3, 10) {
            let a = ids[rng.index(ids.len())];
            let mut b = ids[rng.index(ids.len())];
            while b == a {
                b = ids[rng.index(ids.len())];
            }
            vec![a, b]
        } else {
            // Bias towards recent nodes so chains get long.
            let k = ids.len() - 1 - rng.index(ids.len().min(4));
            vec![ids[k]]
        };
        let id = env
            .commit(&parents, &[put(&format!("f{i}"), &format!("{i}\n"))])
            .await;
        model.add(id, parents);
        ids.push(id);
    }
    (ids, model)
}

async fn check_against_model(seed: u64, n: usize) {
    let env = Env::with_seed(seed).await;
    let (ids, model) = random_dag(&env, seed, n).await;
    let h = env.history();
    for a in &ids {
        let node = h.graph_node(a).await.unwrap();
        assert_eq!(node.generation, model.generation(a), "generation of {a}");
        assert_eq!(node.parents, model.parents(a).to_vec());
        let chain = model.first_parent_chain(a);
        assert_eq!(node.depth as usize, chain.len() - 1);
        for (k, want) in chain.iter().enumerate() {
            assert_eq!(
                h.first_parent_ancestor(a, k as u64).await.unwrap(),
                Some(*want)
            );
        }
        assert_eq!(
            h.first_parent_ancestor(a, chain.len() as u64)
                .await
                .unwrap(),
            None
        );
        for b in &ids {
            assert_eq!(
                h.is_ancestor(a, b).await.unwrap(),
                model.is_ancestor(a, b),
                "is_ancestor({a}, {b})"
            );
            let want: Vec<ObjectId> = model.merge_bases(a, b).into_iter().collect();
            assert_eq!(
                h.merge_bases(a, b).await.unwrap(),
                want,
                "merge_bases({a}, {b})"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, .. ProptestConfig::default() })]

    #[test]
    fn graph_queries_agree_with_the_reference_model(seed in any::<u64>(), n in 2usize..18) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(check_against_model(seed, n));
    }
}

#[tokio::test]
async fn fixed_seeds_agree_with_the_reference_model() {
    for seed in [1, 2, 3, 7, 42] {
        check_against_model(seed, 24).await;
    }
}

#[tokio::test]
async fn criss_cross_has_two_best_bases() {
    let env = Env::new().await;
    let r = env.commit(&[], &[put("a", "1\n")]).await;
    let b1 = env.commit(&[r], &[put("b", "1\n")]).await;
    let c1 = env.commit(&[r], &[put("c", "1\n")]).await;
    let m1 = env.commit(&[b1, c1], &[put("m1", "x\n")]).await;
    let m2 = env.commit(&[c1, b1], &[put("m2", "x\n")]).await;
    let h = env.history();
    let mut want = vec![b1, c1];
    want.sort();
    assert_eq!(h.merge_bases(&m1, &m2).await.unwrap(), want);
    assert_eq!(h.merge_bases(&b1, &c1).await.unwrap(), vec![r]);
    assert_eq!(h.merge_bases(&m1, &b1).await.unwrap(), vec![b1]);
    // Unrelated roots have no base.
    let other = env.commit(&[], &[put("z", "z\n")]).await;
    assert!(h.merge_bases(&m1, &other).await.unwrap().is_empty());
    assert!(!h.is_ancestor(&other, &m1).await.unwrap());
}

/// A first-parent chain of `n` changesets on top of a root.
async fn chain(env: &Env, n: usize) -> Vec<ObjectId> {
    let mut ids = vec![env.commit(&[], &[put("seed", "0\n")]).await];
    for i in 1..=n {
        let last = *ids.last().unwrap();
        ids.push(
            env.commit(&[last], &[put("log.txt", &format!("{i}\n"))])
                .await,
        );
    }
    ids
}

#[tokio::test]
async fn is_ancestor_on_a_2900_commit_linear_gap_reads_log_n_rows() {
    let env = Env::new().await;
    let ids = chain(&env, 2_950).await;
    let measure = |a: ObjectId, b: ObjectId| {
        let env = &env;
        async move {
            let h = env.history();
            let before = env.counters();
            assert!(h.is_ancestor(&a, &b).await.unwrap());
            assert!(!h.is_ancestor(&b, &a).await.unwrap());
            env.counters().since(&before)
        }
    };
    let long = measure(ids[20], ids[2_920]).await;
    let short = measure(ids[20], ids[50]).await;
    // log2(2900) ≈ 11.5: a few rows per level, no changeset rows, no objects.
    let rows = long.reads_of(Table::Derived);
    assert!(rows <= 4 * 12 + 8, "read {rows} graph rows for a 2,900 gap");
    assert!(short.reads_of(Table::Derived) <= 4 * 5 + 8);
    assert_eq!(long.reads_of(Table::Changesets), 0);
    assert_eq!(long.reads_of(Table::Objects), 0);
    // `~n` is O(log n) too.
    let h = env.history();
    let before = env.counters();
    assert_eq!(
        h.first_parent_ancestor(&ids[2_950], 2_900).await.unwrap(),
        Some(ids[50])
    );
    assert!(env.counters().since(&before).reads_of(Table::Derived) <= 14);
}

#[tokio::test]
async fn merge_base_of_branches_off_a_long_trunk_reads_log_n_rows() {
    let env = Env::new().await;
    let ids = chain(&env, 1_500).await;
    let fork = ids[700];
    let mut a = fork;
    let mut b = fork;
    for i in 0..400 {
        a = env.commit(&[a], &[put("a.txt", &format!("{i}\n"))]).await;
        if i < 300 {
            b = env.commit(&[b], &[put("b.txt", &format!("{i}\n"))]).await;
        }
    }
    let tip = *ids.last().unwrap();
    let h = env.history();
    let before = env.counters();
    assert_eq!(h.merge_bases(&a, &b).await.unwrap(), vec![fork]);
    assert_eq!(h.merge_bases(&a, &tip).await.unwrap(), vec![fork]);
    assert_eq!(
        h.merge_bases(&ids[100], &tip).await.unwrap(),
        vec![ids[100]]
    );
    let rows = env.counters().since(&before).reads_of(Table::Derived);
    assert!(rows <= 3 * (4 * 11 + 10), "read {rows} graph rows");
}

#[tokio::test]
async fn missing_graph_rows_are_derived_forward_and_written_back() {
    // TestEnv commits carry only the fixtures' stand-in graph row (version 1).
    let env = bee_testkit::TestEnv::new();
    let r = env.create_repo("acme", "legacy").await.unwrap();
    let mut model = Graph::new();
    let mut ids = Vec::new();
    let mut last = None;
    for i in 0..40 {
        let parents: Vec<ObjectId> = match (last, i % 7) {
            (Some(l), 3) if ids.len() > 4 => vec![l, ids[ids.len() - 4]],
            (Some(l), _) => vec![l],
            (None, _) => vec![],
        };
        let id = env
            .commit_with(
                &r,
                CommitSpec {
                    parents: parents.clone(),
                    changes: vec![bee_testkit::Change::put(&format!("f{i}"), "x")],
                    message: format!("{i}"),
                    change_id: None,
                },
            )
            .await
            .unwrap();
        model.add(id, parents);
        ids.push(id);
        last = Some(id);
    }
    let writer: Writer = env.writer.clone();
    let h = bee_history::History::new(writer, r.org, r.repo);
    let tip = *ids.last().unwrap();
    assert_eq!(
        h.graph_node(&tip).await.unwrap().generation,
        model.generation(&tip)
    );
    // Everything was written back: a fresh handle derives nothing.
    let h2 = bee_history::History::new(env.writer.clone(), r.org, r.repo);
    let before = env.counters.snapshot();
    for a in &ids {
        assert_eq!(
            h2.graph_node(a).await.unwrap().generation,
            model.generation(a)
        );
    }
    assert_eq!(
        env.counters
            .snapshot()
            .since(&before)
            .writes_of(Table::Derived),
        0
    );
    let picks: BTreeSet<usize> = [0, 5, 13, 22, 39].into();
    for &i in &picks {
        for &j in &picks {
            assert_eq!(
                h2.is_ancestor(&ids[i], &ids[j]).await.unwrap(),
                model.is_ancestor(&ids[i], &ids[j])
            );
        }
    }
    // Path history backfills the same way.
    let node = h2.node_at(&tip, None).await.unwrap().unwrap();
    assert_eq!(node.1.children.len(), 40);
}
