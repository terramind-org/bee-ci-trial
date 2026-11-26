//! The derived-data framework and its built-in types over the real write path.

#[path = "../../bee-history/tests/common/mod.rs"]
mod common;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bee_classify::{Class, ClassRules};
use bee_core::{ObjectId, Reason, Result};
use bee_derived::{
    CopyKind, Derived, DerivedType, DiffClassType, KeyKind, LanguageTotals, Leases, Mode, Registry,
};
use bee_history::gitmap::git_mapping_rows;
use bee_history::{BlameOptions, GraphNode, History};
use bee_store::{FsckOptions, Table, fsck};
use common::{Env, del, path, put};

fn engine(env: &Env, registry: Registry) -> Derived {
    let registry = Arc::new(registry);
    let history = History::new(env.env.writer.clone(), env.repo.org, env.repo.repo)
        .with_eager(registry.clone());
    Derived::new(Arc::new(history), registry)
}

#[tokio::test]
async fn registry_eager_rows_are_written_with_each_changeset_and_fsck_is_clean() {
    let env = Env::new().await;
    let registry = Registry::standard();
    let d = engine(&env, registry.clone());
    let meta = env.meta("A <a@x>", "one", None).await;
    env.upload_blobs(&[put("src/a.rs", "fn a() {}\n")]).await;
    let delta = vec![bee_object::DeltaEntry::put(
        path("src/a.rs"),
        bee_core::Mode::File,
        bee_object::blob_id(b"fn a() {}\n"),
        10,
    )];
    let cid = env.env.with_rng(|r| r.change_id());
    let id = d
        .history()
        .create_changeset(vec![], meta, cid, delta, env.env.now())
        .await
        .unwrap();
    let mut names: Vec<String> = registry.eager_types().into_iter().map(|t| t.name).collect();
    names.sort();
    assert_eq!(names, vec!["graph", "path_history"]);
    for t in ["graph", "path_history"] {
        let v = d.get(t, &id).await.unwrap();
        assert!(!v.derived, "{t} was written eagerly");
    }
    let report = fsck(
        env.env.meta_dyn(),
        env.env.blobs.as_ref(),
        env.repo.org,
        &FsckOptions {
            eager: registry.eager_types(),
            ..FsckOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    let g = GraphNode::decode(&d.get("graph", &id).await.unwrap().value).unwrap();
    assert_eq!(g, d.graph(&id).await.unwrap());
}

#[tokio::test]
async fn generic_reads_unknown_types_and_importer_rows() {
    let env = Env::new().await;
    let c = env.commit(&[], &[put("a", "1\n")]).await;
    let d = engine(&env, Registry::standard());
    assert_eq!(
        d.get("nope", &c).await.unwrap_err().reason,
        Reason::NotFound
    );
    assert_eq!(
        d.get("git_mapping", &c).await.unwrap_err().reason,
        Reason::NotFound
    );
    d.history()
        .put_derived(git_mapping_rows(c, &[9; 20]).unwrap())
        .await
        .unwrap();
    assert_eq!(d.get("git_mapping", &c).await.unwrap().value, vec![9; 20]);
    assert_eq!(d.git_oid(&c).await.unwrap(), Some(vec![9; 20]));
    let unknown = ObjectId::new(bee_core::ObjectKind::Changeset, [3; 32]);
    assert_eq!(
        d.language_stats(&unknown).await.unwrap_err().reason,
        Reason::NotFound
    );
    for t in d.registry().types() {
        assert!(!t.name().is_empty());
        assert!(t.version() >= 1);
        let _ = (t.key_kind(), t.content_bearing(), t.mode().as_str());
    }
}

#[tokio::test]
async fn lazy_types_derive_forward_iteratively_and_write_back() {
    let env = Env::new().await;
    let mut last = env.commit(&[], &[put("main.rs", "fn main() {}\n")]).await;
    let mut ids = vec![last];
    for i in 0..300 {
        last = env
            .commit(
                &[last],
                &[put(&format!("m{}.py", i % 50), &format!("x = {i}\n"))],
            )
            .await;
        ids.push(last);
    }
    let d = engine(&env, Registry::standard());
    let before = env.counters();
    let stats = d.language_stats(&last).await.unwrap();
    let writes = env.counters().since(&before).writes_of(Table::Derived);
    assert_eq!(writes, 301, "one value per changeset, written back");
    assert_eq!(
        stats["Rust"],
        LanguageTotals {
            files: 1,
            bytes: 13
        }
    );
    assert_eq!(stats["Python"].files, 50);
    let before = env.counters();
    let mid = d.language_stats(&ids[150]).await.unwrap();
    assert_eq!(env.counters().since(&before).writes_of(Table::Derived), 0);
    assert_eq!(mid["Python"].files, 50);
    // Incremental values equal a recomputation from the whole tree.
    let snap = env.snapshot(&last);
    let py_bytes: u64 = snap
        .iter()
        .filter(|(p, _)| p.ends_with(b".py"))
        .map(|(_, l)| l.size)
        .sum();
    assert_eq!(stats["Python"].bytes, py_bytes);
}

/// Counts derivations; value = 8-byte count of derivations so far.
struct Counting {
    version: u32,
    calls: Arc<AtomicU64>,
}

#[async_trait]
impl DerivedType for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn mode(&self) -> Mode {
        Mode::Lazy
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::Changeset
    }
    fn content_bearing(&self) -> bool {
        false
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        _deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        let g = history.graph_node(key).await?.generation;
        Ok([n.to_be_bytes(), g.to_be_bytes()].concat())
    }
}

#[tokio::test]
async fn version_bumps_rederive_and_old_versions_stay_readable() {
    let env = Env::new().await;
    let c = env.commit(&[], &[put("a", "1\n")]).await;
    let calls = Arc::new(AtomicU64::new(0));
    let mut r1 = Registry::standard();
    r1.register(Arc::new(Counting {
        version: 1,
        calls: calls.clone(),
    }));
    let d1 = engine(&env, r1);
    let v1 = d1.get("counting", &c).await.unwrap();
    assert!(v1.derived);
    assert!(!d1.get("counting", &c).await.unwrap().derived);
    let mut r2 = Registry::standard();
    r2.register(Arc::new(Counting {
        version: 2,
        calls: calls.clone(),
    }));
    let d2 = engine(&env, r2);
    let v2 = d2.get("counting", &c).await.unwrap();
    assert!(v2.derived);
    assert_eq!(v2.version, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        d2.read_stored("counting", 1, &c).await.unwrap(),
        Some(v1.value)
    );
    assert_eq!(d2.read_stored("counting", 3, &c).await.unwrap(), None);
}

#[tokio::test]
async fn leases_deduplicate_concurrent_derivations() {
    let env = Env::new().await;
    let c = env.commit(&[], &[put("a", "1\n")]).await;
    let calls = Arc::new(AtomicU64::new(0));
    let mut r = Registry::standard();
    r.register(Arc::new(Counting {
        version: 1,
        calls: calls.clone(),
    }));
    let leases = Arc::new(Leases::new());
    let d = engine(&env, r).with_leases(leases.clone());
    let (a, b, cc) = tokio::join!(
        d.get("counting", &c),
        d.get("counting", &c),
        d.get("counting", &c)
    );
    let values = [a.unwrap(), b.unwrap(), cc.unwrap()];
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(values.iter().all(|v| v.value == values[0].value));
    assert_eq!(values.iter().filter(|v| v.derived).count(), 1);
    assert_eq!(leases.active(), 0);
}

#[tokio::test]
async fn diff_class_classifies_docs_renames_logic_and_policy_rules() {
    let env = Env::new().await;
    let base = env
        .commit(
            &[],
            &[
                put("docs/guide.md", "hello\n"),
                put("src/lib.rs", "pub fn a() {}\n"),
                put("src/auth/key.rs", "k\n"),
            ],
        )
        .await;
    let docs = env
        .commit(&[base], &[put("docs/guide.md", "hello world\n")])
        .await;
    let mv = env
        .commit(
            &[base],
            &[del("src/lib.rs"), put("src/core.rs", "pub fn a() {}\n")],
        )
        .await;
    let code = env
        .commit(&[base], &[put("src/lib.rs", "pub fn b() {}\n")])
        .await;
    let d = engine(&env, Registry::standard());
    assert_eq!(
        d.diff_class(&docs).await.unwrap().highest,
        Some(Class::Docs)
    );
    let m = d.diff_class(&mv).await.unwrap();
    assert_eq!(m.highest, Some(Class::RenameOnly));
    assert_eq!(m.per_path.len(), 1);
    assert_eq!(m.per_path[0].0, path("src/core.rs"));
    assert_eq!(
        d.diff_class(&code).await.unwrap().highest,
        Some(Class::Logic)
    );
    // Policy rules live under their own key.
    let auth = env.commit(&[base], &[put("src/auth/key.rs", "k2\n")]).await;
    assert_eq!(
        d.diff_class(&auth).await.unwrap().highest,
        Some(Class::Logic)
    );
    let mut r = Registry::standard();
    r.register(Arc::new(DiffClassType::with_rules(
        ClassRules::with_policy(Vec::<&str>::new(), ["src/auth/**"]).unwrap(),
        "epoch-7",
    )));
    let dp = engine(&env, r);
    let v = dp.diff_class(&auth).await.unwrap();
    assert_eq!(v.highest, Some(Class::Sensitive));
    assert_eq!(v.classifier, bee_classify::CLASSIFIER_VERSION);
    // The default-rules value is untouched.
    assert_eq!(
        d.diff_class(&auth).await.unwrap().highest,
        Some(Class::Logic)
    );
}

#[tokio::test]
async fn copy_detection_finds_exact_copies_renames_and_similar_moves() {
    let env = Env::new().await;
    let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    let base = env
        .commit(
            &[],
            &[
                put("a.txt", &body),
                put("lib/util.rs", "fn u() {}\nfn v() {}\n"),
                put("old.txt", &body.replace("line 1\n", "x\n")),
            ],
        )
        .await;
    let c = env
        .commit(
            &[base],
            &[
                put("copy/a.txt", &body),
                del("lib/util.rs"),
                put("src/util.rs", "fn u() {}\nfn v() {}\n"),
                del("old.txt"),
                put("new.txt", &body.replace("line 1\n", "y\n")),
            ],
        )
        .await;
    let d = engine(&env, Registry::standard());
    let copies = d.copies(&c).await.unwrap();
    let got: Vec<(String, String, CopyKind, bool)> = copies
        .iter()
        .map(|r| {
            (
                r.path.to_string(),
                r.source.to_string(),
                r.kind,
                r.similarity.is_none(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("copy/a.txt".into(), "a.txt".into(), CopyKind::Copy, true),
            ("new.txt".into(), "old.txt".into(), CopyKind::Rename, false),
            (
                "src/util.rs".into(),
                "lib/util.rs".into(),
                CopyKind::Rename,
                true
            ),
        ]
    );
    let root = env.commit(&[], &[put("z", "z\n")]).await;
    assert!(d.copies(&root).await.unwrap().is_empty());
}

#[tokio::test]
async fn language_stats_honour_gitattributes_overrides() {
    let env = Env::new().await;
    let c = env
        .commit(
            &[],
            &[
                put("src/main.rs", "fn main() {}\n"),
                put("vendor/lib.js", "var x;\n"),
                put("gen/out.rs", "// generated\n"),
                put("tool", "#!/usr/bin/env python3\nprint(1)\n"),
                put("README.md", "# hi\n"),
                put("weird.conf", "a=b\n"),
            ],
        )
        .await;
    let d = engine(&env, Registry::standard());
    let s = d.language_stats(&c).await.unwrap();
    let names: Vec<&String> = s.keys().collect();
    assert_eq!(names, vec!["Python", "Rust"]);
    assert_eq!(s["Rust"].files, 2);
    let attrs = "gen/** linguist-generated\nvendor/** -linguist-vendored\n*.md linguist-detectable\n*.conf linguist-language=INI\n";
    let c2 = env.commit(&[c], &[put(".gitattributes", attrs)]).await;
    let s2 = d.language_stats(&c2).await.unwrap();
    assert_eq!(s2["Rust"].files, 1);
    assert_eq!(s2["JavaScript"].files, 1);
    assert_eq!(s2["Markdown"].files, 1);
    assert_eq!(s2["INI"].files, 1);
    // Incremental after the attributes: a new generated file stays out.
    let c3 = env
        .commit(&[c2], &[put("gen/more.rs", "x\n"), put("src/b.rs", "y\n")])
        .await;
    let s3 = d.language_stats(&c3).await.unwrap();
    assert_eq!(s3["Rust"].files, 2);
    let c4 = env
        .commit(&[c3], &[del("src/b.rs"), del("src/main.rs")])
        .await;
    assert!(!d.language_stats(&c4).await.unwrap().contains_key("Rust"));
}

#[tokio::test]
async fn blame_through_the_generic_api_matches_the_query() {
    let env = Env::new().await;
    let c1 = env.commit(&[], &[put("f", "a\nb\n")]).await;
    let c2 = env.commit(&[c1], &[put("f", "a\nB\n")]).await;
    let d = engine(&env, Registry::standard());
    let (node, _) = d
        .history()
        .node_at(&c2, Some(&path("f")))
        .await
        .unwrap()
        .unwrap();
    let v = d.get("blame", &node).await.unwrap();
    assert!(v.derived);
    let runs = bee_history::blame::decode_runs(&v.value).unwrap();
    assert_eq!(runs.len(), 2);
    let b = d
        .history()
        .blame(&c2, &path("f"), &BlameOptions::default())
        .await
        .unwrap();
    assert_eq!(
        b.lines().iter().map(|l| l.changeset).collect::<Vec<_>>(),
        vec![c1, c2]
    );
    assert!(!d.get("blame", &node).await.unwrap().derived);
}
