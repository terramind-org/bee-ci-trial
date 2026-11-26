//! Log, path history, blame, diff, revisions, read path and list tree on fixtures, plus the
//! AS-CORE-02 and AS-CORE-04 shaped scenarios.

use crate::common;
use bee_core::{ObjectId, Reason, RefName, Revision};
use bee_history::diff::FileChange;
use bee_history::gitmap::git_mapping_rows;
use bee_history::{
    BlameOptions, ChangeKind, DiffOptions, LineKind, LogOptions, NodeKind, PathContent,
};
use bee_object::Provenance;
use bee_store::{FetchKind, Table};
use bee_testkit::TestRng;
use bee_testkit::model::blame as mblame;
use common::{Env, Spec, del, path, put};
use proptest::prelude::*;

fn ids_of(entries: &[bee_history::PathLogEntry]) -> Vec<ObjectId> {
    entries.iter().map(|e| e.changeset).collect()
}

#[tokio::test]
async fn log_walks_newest_first_with_pages_and_first_parent() {
    let env = Env::new().await;
    let r = env.commit(&[], &[put("a", "0\n")]).await;
    let a1 = env.commit(&[r], &[put("a", "1\n")]).await;
    let b1 = env.commit(&[r], &[put("b", "1\n")]).await;
    let m = env.commit(&[a1, b1], &[put("b", "1\n")]).await;
    let h = env.history();
    let all = h.log(&m, &LogOptions::default()).await.unwrap();
    let ids: Vec<ObjectId> = all.entries.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(ids[0], m);
    assert_eq!(*ids.last().unwrap(), r);
    assert!(all.next_page_token.is_none());
    assert_eq!(all.entries[0].parents, vec![a1, b1]);
    assert!(all.entries[0].meta.is_some());
    let fp = h
        .log(
            &m,
            &LogOptions {
                first_parent: true,
                ..LogOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fp.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![m, a1, r]
    );
    // Pages of one concatenate to the full log.
    let mut paged = Vec::new();
    let mut token = None;
    loop {
        let p = h
            .log(
                &m,
                &LogOptions {
                    limit: 1,
                    page_token: token.clone(),
                    ..LogOptions::default()
                },
            )
            .await
            .unwrap();
        paged.extend(p.entries.iter().map(|e| e.id));
        match p.next_page_token {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    assert_eq!(paged, ids);
    let bad = h
        .log(
            &m,
            &LogOptions {
                page_token: Some("lg1_zz".into()),
                ..LogOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(bad.reason, Reason::InvalidArgument);
}

#[tokio::test]
async fn path_log_continues_through_delete_and_re_add_and_renames() {
    let env = Env::new().await;
    let c1 = env
        .commit(&[], &[put("src/a.rs", "v1\n"), put("other", "o\n")])
        .await;
    let c2 = env.commit(&[c1], &[put("other", "o2\n")]).await;
    let c3 = env.commit(&[c2], &[put("src/a.rs", "v2\n")]).await;
    let c4 = env.commit(&[c3], &[del("src/a.rs")]).await;
    let c5 = env.commit(&[c4], &[put("other", "o3\n")]).await;
    let c6 = env.commit(&[c5], &[put("src/a.rs", "v3\n")]).await;
    let h = env.history();
    let log = h
        .path_log(&c6, Some(&path("src/a.rs")), &LogOptions::default())
        .await
        .unwrap();
    assert_eq!(ids_of(&log.entries), vec![c6, c4, c3, c1]);
    let kinds: Vec<ChangeKind> = log.entries.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ChangeKind::Added,
            ChangeKind::Deleted,
            ChangeKind::Modified,
            ChangeKind::Added
        ]
    );
    // The directory's log sees the same changes.
    let dir = h
        .path_log(&c6, Some(&path("src")), &LogOptions::default())
        .await
        .unwrap();
    assert_eq!(ids_of(&dir.entries), vec![c6, c4, c3, c1]);
    // A path that never existed has no history.
    assert!(
        h.path_log(&c6, Some(&path("nope")), &LogOptions::default())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    // An exact rename links histories.
    let c7 = env
        .commit(&[c6], &[del("src/a.rs"), put("src/b.rs", "v3\n")])
        .await;
    let log = h
        .path_log(&c7, Some(&path("src/b.rs")), &LogOptions::default())
        .await
        .unwrap();
    assert_eq!(ids_of(&log.entries), vec![c7, c6, c4, c3, c1]);
    // Paging through the path log.
    let p1 = h
        .path_log(
            &c7,
            Some(&path("src/b.rs")),
            &LogOptions {
                limit: 2,
                ..LogOptions::default()
            },
        )
        .await
        .unwrap();
    let p2 = h
        .path_log(
            &c7,
            Some(&path("src/b.rs")),
            &LogOptions {
                limit: 10,
                page_token: p1.next_page_token.clone(),
                ..LogOptions::default()
            },
        )
        .await
        .unwrap();
    let mut both = ids_of(&p1.entries);
    both.extend(ids_of(&p2.entries));
    assert_eq!(both, vec![c7, c6, c4, c3, c1]);
    let node = h
        .node_at(&c7, Some(&path("src/a.rs")))
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(node.kind, NodeKind::DeletedFile);
}

#[tokio::test]
async fn path_log_through_merges() {
    let env = Env::new().await;
    let r = env.commit(&[], &[put("f", "base\n")]).await;
    let left = env.commit(&[r], &[put("f", "left\n")]).await;
    let right = env.commit(&[r], &[put("g", "right\n")]).await;
    let right2 = env.commit(&[right], &[put("f", "right\n")]).await;
    // The merge resolves `f` to new content: it is in the log with both histories.
    let m = env
        .commit(
            &[left, right2],
            &[put("f", "merged\n"), put("g", "right\n")],
        )
        .await;
    let h = env.history();
    let log = h
        .path_log(&m, Some(&path("f")), &LogOptions::default())
        .await
        .unwrap();
    assert_eq!(ids_of(&log.entries), vec![m, right2, left, r]);
    let fp = h
        .path_log(
            &m,
            Some(&path("f")),
            &LogOptions {
                first_parent: true,
                ..LogOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ids_of(&fp.entries), vec![m, left, r]);
    // A merge that takes one side's version keeps out of that path's log.
    let m2 = env
        .commit(&[left, right2], &[put("f", "right\n"), put("g", "right\n")])
        .await;
    let log = h
        .path_log(&m2, Some(&path("f")), &LogOptions::default())
        .await
        .unwrap();
    assert_eq!(ids_of(&log.entries), vec![right2, r]);
}

#[tokio::test]
async fn blame_follows_edits_merges_renames_and_ignore_lists() {
    let env = Env::new().await;
    let c1 = env.commit(&[], &[put("f.txt", "a\nb\nc\n")]).await;
    let c2 = env.commit(&[c1], &[put("f.txt", "a\nB\nc\n")]).await;
    let c3 = env.commit(&[c2], &[put("f.txt", "a\nB\nc\nd\n")]).await;
    let h = env.history();
    let b = h
        .blame(&c3, &path("f.txt"), &BlameOptions::default())
        .await
        .unwrap();
    let who: Vec<ObjectId> = b.lines().iter().map(|l| l.changeset).collect();
    assert_eq!(who, vec![c1, c2, c1, c3]);
    assert_eq!(b.line_count(), 4);
    assert_eq!(b.line(2).unwrap().changeset, c2);
    assert!(b.line(5).is_none());
    assert_eq!(b.metas.len(), 3);
    // Ignoring c2 attributes its replaced line to the line it replaced.
    let ig = h
        .blame(
            &c3,
            &path("f.txt"),
            &BlameOptions {
                ignore_revs: vec![c2],
            },
        )
        .await
        .unwrap();
    let who: Vec<ObjectId> = ig.lines().iter().map(|l| l.changeset).collect();
    assert_eq!(who, vec![c1, c1, c1, c3]);
    // Merge: lines from the second parent keep their attribution.
    let side = env.commit(&[c1], &[put("f.txt", "a\nb\nc\nside\n")]).await;
    let m = env
        .commit(&[c3, side], &[put("f.txt", "a\nB\nc\nd\nside\n")])
        .await;
    let bm = h
        .blame(&m, &path("f.txt"), &BlameOptions::default())
        .await
        .unwrap();
    let who: Vec<ObjectId> = bm.lines().iter().map(|l| l.changeset).collect();
    assert_eq!(who, vec![c1, c2, c1, c3, side]);
    // Exact rename: blame follows.
    let mv = env
        .commit(&[m], &[del("f.txt"), put("g.txt", "a\nB\nc\nd\nside\n")])
        .await;
    let bg = h
        .blame(&mv, &path("g.txt"), &BlameOptions::default())
        .await
        .unwrap();
    assert_eq!(
        bg.lines().iter().map(|l| l.changeset).collect::<Vec<_>>(),
        who
    );
    assert_eq!(bg.ranges[0].path, path("f.txt"));
    // Delete and re-add starts afresh.
    let gone = env.commit(&[mv], &[del("g.txt")]).await;
    let back = env.commit(&[gone], &[put("g.txt", "a\n")]).await;
    let bb = h
        .blame(&back, &path("g.txt"), &BlameOptions::default())
        .await
        .unwrap();
    assert_eq!(bb.lines()[0].changeset, back);
    // Not a file.
    assert_eq!(
        h.blame(&back, &path("nope"), &BlameOptions::default())
            .await
            .unwrap_err()
            .reason,
        Reason::NotFound
    );
    // `.git-blame-ignore-revs` resolves changeset ids and git ids.
    h.put_derived(git_mapping_rows(c1, &[0xab; 20]).unwrap())
        .await
        .unwrap();
    let list = format!(
        "# formatting\n{c2}\n{}  # import\nnot-a-rev\n",
        "ab".repeat(20)
    );
    let with = env
        .commit(&[back], &[put(".git-blame-ignore-revs", &list)])
        .await;
    let mut want = vec![c1, c2];
    want.sort();
    assert_eq!(h.blame_ignore_revs(&with).await.unwrap(), want);
    assert!(h.blame_ignore_revs(&c1).await.unwrap().is_empty());
}

/// A random linear history of one file where every new line is unique (so the longest common
/// subsequence between versions is unique and Myers and the model's LCS agree), with
/// unrelated commits and deletes mixed in.
async fn blame_vs_model(seed: u64, steps: usize) {
    let env = Env::with_seed(seed).await;
    let mut rng = TestRng::seed(seed);
    let mut lines: Vec<String> = Vec::new();
    let mut present = false;
    let mut history: Vec<(ObjectId, Option<Vec<u8>>)> = Vec::new();
    let mut parent: Option<ObjectId> = None;
    let mut fresh = 0u64;
    for _ in 0..steps {
        let roll = rng.below(10);
        let changes = if roll == 0 && present {
            present = false;
            lines.clear();
            vec![del("f.txt")]
        } else if roll == 1 {
            vec![put("noise", &format!("{}\n", rng.next_u64()))]
        } else {
            present = true;
            let edits = 1 + rng.below(3);
            for _ in 0..edits {
                fresh += 1;
                let line = format!("line {fresh}");
                match rng.below(3) {
                    0 if !lines.is_empty() => {
                        let i = rng.index(lines.len());
                        lines.remove(i);
                    }
                    1 if !lines.is_empty() => {
                        let i = rng.index(lines.len());
                        lines[i] = line;
                    }
                    _ => {
                        let i = rng.index(lines.len() + 1);
                        lines.insert(i, line);
                    }
                }
            }
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            vec![put("f.txt", &text)]
        };
        let id = env
            .commit(&parent.into_iter().collect::<Vec<_>>(), &changes)
            .await;
        parent = Some(id);
        let content = env.snapshot(&id).get(&b"f.txt"[..]).map(|_| {
            lines
                .iter()
                .map(|l| format!("{l}\n"))
                .collect::<String>()
                .into_bytes()
        });
        history.push((id, content));
    }
    let tip = parent.unwrap();
    let h = env.history();
    let want = mblame::blame(&history);
    match h
        .blame(&tip, &path("f.txt"), &BlameOptions::default())
        .await
    {
        Ok(got) => {
            let who: Vec<ObjectId> = got.lines().iter().map(|l| l.changeset).collect();
            let model: Vec<ObjectId> = want.iter().map(|(c, _)| *c).collect();
            assert_eq!(who, model, "seed {seed}");
        }
        Err(e) => {
            assert_eq!(e.reason, Reason::NotFound);
            assert!(want.is_empty() || !present);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn blame_agrees_with_the_reference_model(seed in any::<u64>(), steps in 1usize..30) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(blame_vs_model(seed, steps));
    }
}

#[tokio::test]
async fn blame_values_are_derived_once_and_reused() {
    let env = Env::new().await;
    let mut last = env.commit(&[], &[put("f", "0\n")]).await;
    for i in 1..50 {
        let text: String = (0..=i).map(|k| format!("{k}\n")).collect();
        last = env.commit(&[last], &[put("f", &text)]).await;
    }
    let h = env.history();
    h.blame(&last, &path("f"), &BlameOptions::default())
        .await
        .unwrap();
    let before = env.counters();
    let h2 = env.history();
    let b = h2
        .blame(&last, &path("f"), &BlameOptions::default())
        .await
        .unwrap();
    let d = env.counters().since(&before);
    assert_eq!(b.line_count(), 50);
    assert_eq!(d.writes_of(Table::Derived), 0, "nothing re-derived");
    assert_eq!(d.blob.gets, 0, "cached blame reads no contents");
}

#[tokio::test]
async fn diff_with_hunks_renames_scopes_and_whitespace() {
    let env = Env::new().await;
    let a = env
        .commit(
            &[],
            &[
                put("src/x.rs", "fn a() {}\nfn b() {}\n"),
                put("docs/r.md", "one\ntwo\nthree\nfour\n"),
                put("bin", "\0\x01"),
            ],
        )
        .await;
    let b = env
        .commit(
            &[a],
            &[
                put("src/x.rs", "fn a() {}\nfn  b() {}\n"),
                del("docs/r.md"),
                put("docs/readme.md", "one\ntwo\nthree\nFOUR\n"),
                put("bin", "\0\x02"),
                put("new.txt", "hi\n"),
            ],
        )
        .await;
    let h = env.history();
    let plain = h.diff(Some(&a), &b, &DiffOptions::default()).await.unwrap();
    let paths: Vec<String> = plain.iter().map(|f| f.path.to_string()).collect();
    assert_eq!(
        paths,
        vec!["bin", "docs/r.md", "docs/readme.md", "new.txt", "src/x.rs"]
    );
    assert_eq!(plain[1].change, FileChange::Deleted);
    assert!(plain.iter().all(|f| f.hunks.is_empty()));
    let full = h
        .diff(
            Some(&a),
            &b,
            &DiffOptions {
                hunks: true,
                detect_renames: true,
                ..DiffOptions::default()
            },
        )
        .await
        .unwrap();
    let readme = full
        .iter()
        .find(|f| f.path == path("docs/readme.md"))
        .unwrap();
    assert!(
        matches!(&readme.change, FileChange::Renamed { from, similarity: Some(_) } if *from == path("docs/r.md"))
    );
    assert_eq!((readme.added, readme.removed), (1, 1));
    assert!(full.iter().all(|f| f.path != path("docs/r.md")));
    assert!(full.iter().find(|f| f.path == path("bin")).unwrap().binary);
    let x = full.iter().find(|f| f.path == path("src/x.rs")).unwrap();
    assert_eq!(x.hunks.len(), 1);
    assert!(x.hunks[0].lines.iter().any(|l| l.kind == LineKind::Added));
    let ws = h
        .diff(
            Some(&a),
            &b,
            &DiffOptions {
                hunks: true,
                ignore_whitespace: true,
                paths: vec![path("src")],
                ..DiffOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ws.len(), 1);
    assert!(ws[0].hunks.is_empty());
    // A scope that is a file, and diff from the empty tree.
    let one = h
        .diff(
            Some(&a),
            &b,
            &DiffOptions {
                paths: vec![path("new.txt")],
                ..DiffOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].change, FileChange::Added);
    assert_eq!(
        h.diff(None, &a, &DiffOptions::default())
            .await
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn revisions_resolve_refs_versions_suffixes_prefixes_change_ids_and_git() {
    let env = Env::new().await;
    let c1 = env.commit(&[], &[put("a", "1\n")]).await;
    let c2 = env.commit(&[c1], &[put("a", "2\n")]).await;
    let side = env.commit(&[c1], &[put("b", "1\n")]).await;
    let m = env.commit(&[c2, side], &[put("b", "1\n")]).await;
    env.set_ref("main", c2).await;
    env.set_ref("main", m).await;
    let h = env.history();
    let r = |s: &str| {
        let h = &h;
        let s = s.to_string();
        async move { h.resolve_str(&s, None).await }
    };
    assert_eq!(r("main").await.unwrap(), m);
    assert_eq!(r("main@1").await.unwrap(), c2);
    assert_eq!(r("main~1").await.unwrap(), c2);
    assert_eq!(r("main~2").await.unwrap(), c1);
    assert_eq!(r("main^2").await.unwrap(), side);
    assert_eq!(r("main^0").await.unwrap(), m);
    assert_eq!(r("main^2~1").await.unwrap(), c1);
    assert_eq!(r("main~3").await.unwrap_err().reason, Reason::NotFound);
    assert_eq!(r("main^3").await.unwrap_err().reason, Reason::NotFound);
    assert_eq!(r("main@9").await.unwrap_err().reason, Reason::NotFound);
    assert_eq!(r("nope").await.unwrap_err().reason, Reason::NotFound);
    assert_eq!(r(&c1.to_text()).await.unwrap(), c1);
    let hex = c1.to_text();
    assert_eq!(r(&hex[..14]).await.unwrap(), c1);
    let absent = format!("c1_{}", "0".repeat(64));
    assert_eq!(r(&absent).await.unwrap_err().reason, Reason::NotFound);
    assert_eq!(r("main~").await.unwrap(), c2);
    // A prefix matching nothing is NOT_FOUND (several matches are AMBIGUOUS_PREFIX via
    // `ObjectIdPrefix::resolve`, exercised with forged ids in bee-core).
    let hex_zero = format!("c1_{}", "0".repeat(12));
    assert_eq!(r(&hex_zero).await.unwrap_err().reason, Reason::NotFound);
    // ChangeIds: on a ref via the landed index, or unique in the repo.
    let cid = *h.changeset(&c2).await.unwrap().change_id();
    assert_eq!(r(&cid.to_text()).await.unwrap(), c2);
    // An amended change (same ChangeId twice) is ambiguous without a ref.
    let amended = env
        .commit_spec(Spec {
            parents: vec![c1],
            changes: vec![put("a", "2b\n")],
            change_id: Some(cid),
            ..Spec::default()
        })
        .await;
    let e = r(&cid.to_text()).await.unwrap_err();
    assert_eq!(e.reason, Reason::AmbiguousPrefix);
    assert!(e.payload.contains_key("candidates"));
    assert_ne!(amended, c2);
    let main = RefName::new("main").unwrap();
    assert_eq!(
        h.resolve(&Revision::parse(&cid.to_text()).unwrap(), Some(&main))
            .await
            .unwrap_err()
            .reason,
        Reason::NotFound
    );
    // git: ids through git_mapping rows.
    h.put_derived(git_mapping_rows(c2, &[0x1f; 20]).unwrap())
        .await
        .unwrap();
    let full = "1f".repeat(20);
    assert_eq!(r(&format!("git:{full}")).await.unwrap(), c2);
    assert_eq!(r(&format!("git:{}", &full[..9])).await.unwrap(), c2);
    assert_eq!(r("git:abcdef0").await.unwrap_err().reason, Reason::NotFound);
    assert_eq!(h.git_oid_of(&c2).await.unwrap(), Some(vec![0x1f; 20]));
}

#[tokio::test]
async fn read_path_and_paginated_list_tree() {
    let env = Env::new().await;
    let c = env
        .commit(
            &[],
            &[
                put("a/x", "1\n"),
                put("a/y/z", "2\n"),
                put("a.txt", "3\n"),
                put("b", "4\n"),
            ],
        )
        .await;
    let h = env.history();
    match h.read_path(&c, Some(&path("a/y/z"))).await.unwrap() {
        PathContent::File { content, item } => {
            assert_eq!(content.bytes_or_empty(), b"2\n");
            assert_eq!(item.size, 2);
        }
        other => panic!("{other:?}"),
    }
    match h.read_path(&c, None).await.unwrap() {
        PathContent::Dir { entries, .. } => assert_eq!(entries.len(), 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        h.read_path(&c, Some(&path("q"))).await.unwrap_err().reason,
        Reason::NotFound
    );
    let all = h.list_tree(&c, None, true, 100, None).await.unwrap();
    let names: Vec<String> = all.items.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(names, vec!["a", "a/x", "a/y", "a/y/z", "a.txt", "b"]);
    let mut paged = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let p = h
            .list_tree(&c, None, true, 2, token.as_deref())
            .await
            .unwrap();
        paged.extend(p.items.iter().map(|i| i.path.to_string()));
        match p.next_page_token {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    assert_eq!(paged, names);
    // Every page size, including ones where a page ends on a directory, lists everything.
    for size in 1..=names.len() + 1 {
        let mut paged = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let p = h
                .list_tree(&c, None, true, size, token.as_deref())
                .await
                .unwrap();
            paged.extend(p.items.iter().map(|i| i.path.to_string()));
            match p.next_page_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        assert_eq!(paged, names, "page size {size}");
    }
    let sub = h
        .list_tree(&c, Some(&path("a")), false, 100, None)
        .await
        .unwrap();
    assert_eq!(sub.items.len(), 2);
    assert!(
        h.list_tree(&c, Some(&path("b")), false, 10, None)
            .await
            .is_err()
    );
    assert!(
        h.list_tree(&c, None, true, 10, Some("bogus"))
            .await
            .is_err()
    );
}

/// AS-CORE-02: find which change introduced a line and who (or which agent) made it.
#[tokio::test]
async fn as_core_02_blame_names_the_change_and_agent_behind_a_line() {
    let env = Env::new().await;
    let c1 = env
        .commit_spec(Spec {
            changes: vec![put("src/auth/login.rs", "fn login() {\n    check();\n}\n")],
            author: Some("Ada <ada@example.com>".into()),
            message: Some("Add login".into()),
            ..Spec::default()
        })
        .await;
    env.env.clock.advance_ms(60_000);
    let provenance = Provenance {
        principal_chain: vec!["agent:fixer".into(), "user:ada".into()],
        runtime: Some("claude-code/3.2".into()),
        model: Some("model-x".into()),
        session: Some("sess-42".into()),
        ..Provenance::default()
    };
    let c2 = env
        .commit_spec(Spec {
            parents: vec![c1],
            changes: vec![put(
                "src/auth/login.rs",
                "fn login() {\n    check_mfa();\n}\n",
            )],
            author: Some("Fixer Agent <fixer@agents>".into()),
            message: Some("Require MFA".into()),
            provenance: Some(provenance.clone()),
            ..Spec::default()
        })
        .await;
    env.set_ref("main", c2).await;
    let h = env.history();
    let tip = h.resolve_str("main", None).await.unwrap();
    let b = h
        .blame(&tip, &path("src/auth/login.rs"), &BlameOptions::default())
        .await
        .unwrap();
    let line = b.lines().into_iter().find(|l| l.line == 2).unwrap();
    assert_eq!(line.changeset, c2);
    let meta = b.metas[&c2].as_ref().unwrap();
    assert_eq!(meta.author, "Fixer Agent <fixer@agents>");
    assert_eq!(meta.message, "Require MFA");
    assert_eq!(meta.provenance.as_ref().unwrap(), &provenance);
    assert_eq!(b.line(1).unwrap().changeset, c1);
    assert_eq!(
        b.metas[&c1].as_ref().unwrap().author,
        "Ada <ada@example.com>"
    );
}

/// AS-CORE-04: what changed in `src/auth` since yesterday, without fetching file contents.
#[tokio::test]
async fn as_core_04_changes_under_a_directory_since_yesterday_read_no_contents() {
    let env = Env::new().await;
    let day = 24 * 60 * 60 * 1000;
    env.env.clock.set(10 * day);
    let old = env
        .commit(
            &[],
            &[
                put("src/auth/login.rs", "a\n"),
                put("src/auth/token.rs", "t\n"),
                put("src/ui/app.rs", "u\n"),
            ],
        )
        .await;
    env.env.clock.set(12 * day);
    let yesterday_secs = (11 * day / 1000) as i64;
    let c1 = env.commit(&[old], &[put("src/auth/login.rs", "b\n")]).await;
    let c2 = env.commit(&[c1], &[put("src/ui/app.rs", "v\n")]).await;
    let c3 = env
        .commit(
            &[c2],
            &[del("src/auth/token.rs"), put("src/auth/mfa/totp.rs", "m\n")],
        )
        .await;
    env.set_ref("main", c3).await;
    let h = env.history();
    let tip = h.resolve_str("main", None).await.unwrap();
    let before = env.counters();
    let log = h
        .path_log(
            &tip,
            Some(&path("src/auth")),
            &LogOptions {
                since: Some(yesterday_secs),
                changed_paths: true,
                ..LogOptions::default()
            },
        )
        .await
        .unwrap();
    let d = env.counters().since(&before);
    assert_eq!(ids_of(&log.entries), vec![c3, c1]);
    let changed: Vec<(String, ChangeKind)> = log.entries[0]
        .changed
        .iter()
        .map(|c| (c.path.to_string(), c.kind))
        .collect();
    assert_eq!(
        changed,
        vec![
            ("src/auth/mfa/totp.rs".to_string(), ChangeKind::Added),
            ("src/auth/token.rs".to_string(), ChangeKind::Deleted),
        ]
    );
    assert_eq!(log.entries[1].changed[0].path, path("src/auth/login.rs"));
    assert_eq!(d.fetches_of(FetchKind::Blob), 0);
    assert_eq!(d.blob.gets, 0);
    assert_eq!(d.fetches_of(FetchKind::Tree), 0, "no trees either");
}
