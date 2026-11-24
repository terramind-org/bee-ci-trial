//! Tree-level rebase, merge and land helpers (SPEC §7.3, §7.4).

use crate::common;
use bee_core::{ObjectId, Reason, Subcode};
use bee_history::{
    ConflictSet, MergeBase, MergeOptions, MergeOutcome, RebaseOptions, RebaseOutcome, StackOutcome,
};
use bee_merge::ConflictKind;
use bee_store::FetchKind;
use bee_testkit::fixtures::apply_changes;
use bee_testkit::model::tree::{self as mtree, FileMap};
use common::{Env, del, path, put};

async fn root_of(env: &Env, id: &ObjectId) -> ObjectId {
    *env.history().changeset(id).await.unwrap().root()
}

fn expect(base: &FileMap, changes: &[bee_testkit::Change]) -> FileMap {
    apply_changes(base, changes).unwrap()
}

async fn file(env: &Env, id: &ObjectId, p: &str) -> Option<Vec<u8>> {
    match env.history().read_path(id, Some(&path(p))).await {
        Ok(bee_history::PathContent::File { content, .. }) => {
            Some(content.bytes_or_empty().to_vec())
        }
        _ => None,
    }
}

fn rebased(o: RebaseOutcome) -> ObjectId {
    match o {
        RebaseOutcome::Rebased(id) => id,
        other => panic!("expected a rebase, got {other:?}"),
    }
}

fn conflicts(o: RebaseOutcome) -> ConflictSet {
    match o {
        RebaseOutcome::Conflicts(c) => c,
        other => panic!("expected conflicts, got {other:?}"),
    }
}

#[tokio::test]
async fn rebase_over_an_unrelated_tip_reads_zero_blobs() {
    let env = Env::new().await;
    let files: Vec<bee_testkit::Change> = (0..200)
        .map(|i| put(&format!("d{}/f{i}.txt", i % 10), &format!("{i}\n")))
        .collect();
    let base = env.commit(&[], &files).await;
    let c = env
        .commit(
            &[base],
            &[put("d3/f3.txt", "mine\n"), put("new/a.txt", "a\n")],
        )
        .await;
    let mut tip = base;
    for i in 0..5 {
        tip = env
            .commit(
                &[tip],
                &[put(&format!("d{i}/f{}.txt", 10 * i + 5), "theirs\n")],
            )
            .await;
    }
    let h = env.history();
    let before = env.counters();
    let out = h
        .rebase(&c, &tip, &RebaseOptions::default(), env.env.now())
        .await
        .unwrap();
    let d = env.counters().since(&before);
    let id = rebased(out);
    assert_eq!(d.fetches_of(FetchKind::Blob), 0);
    assert_eq!(d.blob.gets, 0);
    let want = expect(
        &env.snapshot(&tip),
        &[put("d3/f3.txt", "mine\n"), put("new/a.txt", "a\n")],
    );
    assert_eq!(root_of(&env, &id).await, mtree::root_id(&want));
    let cs = h.changeset(&id).await.unwrap();
    let orig = h.changeset(&c).await.unwrap();
    assert_eq!(cs.meta(), orig.meta());
    assert_eq!(cs.change_id(), orig.change_id());
    assert_eq!(cs.parents(), &[tip]);
    // Deterministic: the same rebase gives the same id.
    let again = rebased(
        env.history()
            .rebase(&c, &tip, &RebaseOptions::default(), 99)
            .await
            .unwrap(),
    );
    assert_eq!(again, id);
    // Already on the target: unchanged.
    assert_eq!(
        h.rebase(&c, &base, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
        RebaseOutcome::Unchanged(c)
    );
}

#[tokio::test]
async fn rebase_merges_lines_reports_conflicts_and_empty_changes() {
    let env = Env::new().await;
    let base = env
        .commit(&[], &[put("f.txt", "1\n2\n3\n4\n5\n6\n7\n")])
        .await;
    let mine = env
        .commit(&[base], &[put("f.txt", "ONE\n2\n3\n4\n5\n6\n7\n")])
        .await;
    let theirs = env
        .commit(&[base], &[put("f.txt", "1\n2\n3\n4\n5\n6\nSEVEN\n")])
        .await;
    let h = env.history();
    let id = rebased(
        h.rebase(&mine, &theirs, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
    );
    assert_eq!(
        file(&env, &id, "f.txt").await.unwrap(),
        b"ONE\n2\n3\n4\n5\n6\nSEVEN\n"
    );

    let clash = env
        .commit(&[base], &[put("f.txt", "uno\n2\n3\n4\n5\n6\n7\n")])
        .await;
    let c = conflicts(
        h.rebase(&clash, &mine, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
    );
    assert_eq!(c.conflicts.len(), 1);
    let pc = &c.conflicts[0];
    assert_eq!(pc.path, path("f.txt"));
    assert_eq!(pc.kind, ConflictKind::Content);
    assert_eq!(pc.hunks.len(), 1);
    assert!(pc.base.is_some() && pc.ours.is_some() && pc.theirs.is_some());
    let err = c.to_error();
    assert_eq!(err.reason, Reason::MergeConflict);
    assert!(err.payload.contains_key("conflicts"));

    // The same change already upstream: empty.
    let same = env
        .commit(&[base], &[put("f.txt", "ONE\n2\n3\n4\n5\n6\n7\n")])
        .await;
    let e = h
        .rebase(&same, &mine, &RebaseOptions::default(), 0)
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::EmptyChange);
    let allowed = RebaseOptions {
        allow_empty: true,
        ..RebaseOptions::default()
    };
    let id = rebased(h.rebase(&same, &mine, &allowed, 0).await.unwrap());
    assert_eq!(root_of(&env, &id).await, root_of(&env, &mine).await);

    // Modify/delete.
    let gone = env.commit(&[base], &[del("f.txt")]).await;
    let c = conflicts(
        h.rebase(&clash, &gone, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
    );
    assert_eq!(c.conflicts[0].kind, ConflictKind::ModifyDelete);

    // Multi-parent changesets do not rebase.
    let m = env.commit(&[mine, theirs], &[put("g", "g\n")]).await;
    let e = h
        .rebase(&m, &base, &RebaseOptions::default(), 0)
        .await
        .unwrap_err();
    assert!(e.is_invalid(Subcode::InvalidParents));
}

/// AS-MERGE-02: land a change across a rename made by another agent without a conflict.
#[tokio::test]
async fn rebase_follows_a_rename_made_upstream() {
    let env = Env::new().await;
    let text = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n";
    let base = env
        .commit(&[], &[put("src/old.rs", text), put("x", "x\n")])
        .await;
    let renamed = env
        .commit(&[base], &[del("src/old.rs"), put("src/new.rs", text)])
        .await;
    let edit = env
        .commit(
            &[base],
            &[put(
                "src/old.rs",
                "fn a() {}\nfn b() { todo() }\nfn c() {}\nfn d() {}\n",
            )],
        )
        .await;
    let h = env.history();
    let id = rebased(
        h.rebase(&edit, &renamed, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
    );
    assert_eq!(
        file(&env, &id, "src/new.rs").await.unwrap(),
        b"fn a() {}\nfn b() { todo() }\nfn c() {}\nfn d() {}\n"
    );
    assert!(file(&env, &id, "src/old.rs").await.is_none());
    // A rename with edits upstream is followed by similarity.
    let moved_edited = env
        .commit(
            &[base],
            &[
                del("src/old.rs"),
                put(
                    "src/moved.rs",
                    "fn a() {}\nfn b() {}\nfn c() {}\nfn D() {}\n",
                ),
            ],
        )
        .await;
    let id = rebased(
        h.rebase(&edit, &moved_edited, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
    );
    assert_eq!(
        file(&env, &id, "src/moved.rs").await.unwrap(),
        b"fn a() {}\nfn b() { todo() }\nfn c() {}\nfn D() {}\n"
    );
}

#[tokio::test]
async fn rebase_detects_file_directory_collisions_outside_the_delta() {
    let env = Env::new().await;
    let base = env.commit(&[], &[put("a", "a\n")]).await;
    let dir = env.commit(&[base], &[put("d/x", "x\n")]).await;
    let file_at_d = env.commit(&[base], &[put("d", "file\n")]).await;
    let c = conflicts(
        env.history()
            .rebase(&file_at_d, &dir, &RebaseOptions::default(), 0)
            .await
            .unwrap(),
    );
    let kinds: Vec<(String, ConflictKind)> = c
        .conflicts
        .iter()
        .map(|c| (c.path.to_string(), c.kind))
        .collect();
    assert!(kinds.contains(&("d".into(), ConflictKind::FileDirectory)));
    assert!(kinds.contains(&("d/x".into(), ConflictKind::FileDirectory)));
}

#[tokio::test]
async fn merge_clean_conflicting_unrelated_and_rename_aware() {
    let env = Env::new().await;
    let base = env
        .commit(&[], &[put("f", "1\n2\n3\n4\n5\n"), put("keep", "k\n")])
        .await;
    let ours = env
        .commit(&[base], &[put("f", "one\n2\n3\n4\n5\n"), put("o", "o\n")])
        .await;
    let theirs = env
        .commit(&[base], &[put("f", "1\n2\n3\n4\nfive\n"), put("t", "t\n")])
        .await;
    let h = env.history();
    let meta = env.meta("Merger <m@x>", "merge", None).await;
    let cid = env.env.with_rng(|r| r.change_id());
    let MergeOutcome::Merged { id, base: b } = h
        .merge(&ours, &theirs, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!("conflict")
    };
    assert_eq!(b, MergeBase::Unique(base));
    let want = expect(
        &env.snapshot(&ours),
        &[put("f", "one\n2\n3\n4\nfive\n"), put("t", "t\n")],
    );
    assert_eq!(root_of(&env, &id).await, mtree::root_id(&want));
    assert_eq!(h.changeset(&id).await.unwrap().parents(), &[ours, theirs]);
    assert!(h.is_ancestor(&theirs, &id).await.unwrap());

    // Conflicting.
    let clash = env.commit(&[base], &[put("f", "uno\n2\n3\n4\n5\n")]).await;
    match h
        .merge(&ours, &clash, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap()
    {
        MergeOutcome::Conflicts(c) => {
            assert_eq!(c.conflicts[0].kind, ConflictKind::Content);
            assert_eq!(c.base, MergeBase::Unique(base));
        }
        other => panic!("{other:?}"),
    }

    // Already merged: nothing to do.
    let e = h
        .merge(&id, &theirs, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::EmptyChange);

    // Unrelated histories merge over the empty tree.
    let other = env.commit(&[], &[put("z", "z\n")]).await;
    let MergeOutcome::Merged { base: b, id: u } = h
        .merge(&ours, &other, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!("conflict")
    };
    assert_eq!(b, MergeBase::None);
    assert_eq!(file(&env, &u, "z").await.unwrap(), b"z\n");

    // Ours renamed, theirs edited: the edit lands at the new path.
    let mv = env
        .commit(&[base], &[del("f"), put("g", "1\n2\n3\n4\n5\n")])
        .await;
    let MergeOutcome::Merged { id: r, .. } = h
        .merge(&mv, &theirs, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!("conflict")
    };
    assert_eq!(file(&env, &r, "g").await.unwrap(), b"1\n2\n3\n4\nfive\n");
    assert!(file(&env, &r, "f").await.is_none());
}

#[tokio::test]
async fn merge_uses_copy_sources_outside_the_changed_paths() {
    let env = Env::new().await;
    let lib: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    let base = env.commit(&[], &[put("lib.txt", &lib)]).await;
    let ours_copy = lib.replace("line 1\n", "LINE 1\n");
    let theirs_copy = lib.replace("line 12\n", "LINE 12\n");
    let ours = env.commit(&[base], &[put("copy.txt", &ours_copy)]).await;
    let theirs = env.commit(&[base], &[put("copy.txt", &theirs_copy)]).await;
    let h = env.history();
    let meta = env.meta("M <m@x>", "merge copies", None).await;
    let cid = env.env.with_rng(|r| r.change_id());
    let MergeOutcome::Merged { id, .. } = h
        .merge(&ours, &theirs, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!("copy-aware add/add should merge cleanly")
    };
    let both = lib
        .replace("line 1\n", "LINE 1\n")
        .replace("line 12\n", "LINE 12\n");
    assert_eq!(
        file(&env, &id, "copy.txt").await.unwrap(),
        both.into_bytes()
    );
}

#[tokio::test]
async fn criss_cross_merge_uses_a_virtual_base() {
    let env = Env::new().await;
    let r = env
        .commit(&[], &[put("f", "a\nb\nc\nd\ne\n"), put("g", "g\n")])
        .await;
    let x = env.commit(&[r], &[put("f", "A\nb\nc\nd\ne\n")]).await;
    let y = env.commit(&[r], &[put("f", "a\nb\nc\nd\nE\n")]).await;
    // Criss-cross: each side merges the other.
    let both = "A\nb\nc\nd\nE\n";
    let m1 = env.commit(&[x, y], &[put("f", both)]).await;
    let m2 = env.commit(&[y, x], &[put("f", both)]).await;
    let o = env.commit(&[m1], &[put("f", "A\nb\nC\nd\nE\n")]).await;
    let t = env.commit(&[m2], &[put("g", "G\n")]).await;
    let h = env.history();
    let meta = env.meta("M <m@x>", "criss-cross", None).await;
    let cid = env.env.with_rng(|r| r.change_id());
    // m1 and m2 themselves form a criss-cross: two best bases.
    let mut xy = vec![x, y];
    xy.sort();
    assert_eq!(h.merge_bases(&m1, &m2).await.unwrap(), xy);
    match h
        .merge(&o, &t, meta, cid, &MergeOptions::default(), 0)
        .await
        .unwrap()
    {
        MergeOutcome::Merged { id, base } => {
            // o and t descend from m1 and m2, whose best common ancestors are x and y.
            let mut bases = vec![x, y];
            bases.sort();
            assert_eq!(base, MergeBase::Virtual(bases));
            assert_eq!(file(&env, &id, "f").await.unwrap(), b"A\nb\nC\nd\nE\n");
            assert_eq!(file(&env, &id, "g").await.unwrap(), b"G\n");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn land_stack_and_rebase_stack() {
    let env = Env::new().await;
    let base = env
        .commit(&[], &[put("a", "a\n"), put("b", "b\n"), put("c", "c\n")])
        .await;
    let s1 = env.commit(&[base], &[put("a", "a1\n")]).await;
    let s2 = env.commit(&[s1], &[put("b", "b1\n")]).await;
    let s3 = env.commit(&[s2], &[put("c", "c1\n")]).await;
    let main = env.commit(&[base], &[put("z", "z\n")]).await;
    let h = env.history();
    let stack = h.land_stack(&s3, &main).await.unwrap();
    assert_eq!(stack, vec![s1, s2, s3]);
    let StackOutcome::Rebased(steps) = h
        .rebase_stack(&stack, &main, &RebaseOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!("conflict")
    };
    assert_eq!(steps.len(), 3);
    assert_eq!(
        h.changeset(&steps[0].result).await.unwrap().parents(),
        &[main]
    );
    assert_eq!(
        h.changeset(&steps[2].result).await.unwrap().parents(),
        &[steps[1].result]
    );
    for (s, src) in steps.iter().zip([s1, s2, s3]) {
        assert_eq!(s.source, src);
        assert_eq!(s.change_id, *h.changeset(&src).await.unwrap().change_id());
    }
    let want = expect(
        &env.snapshot(&main),
        &[put("a", "a1\n"), put("b", "b1\n"), put("c", "c1\n")],
    );
    assert_eq!(root_of(&env, &steps[2].result).await, mtree::root_id(&want));
    // Already landed: the stack is empty.
    assert!(
        h.land_stack(&steps[2].result, &steps[2].result)
            .await
            .unwrap()
            .is_empty()
    );
    // A stack on the tip itself rebases to itself.
    let StackOutcome::Rebased(same) = h
        .rebase_stack(&[s1, s2], &base, &RebaseOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(same[1].result, s2);
    // A conflicting member stops the stack.
    let clash = env.commit(&[main], &[put("b", "other\n")]).await;
    match h
        .rebase_stack(&stack, &clash, &RebaseOptions::default(), 0)
        .await
        .unwrap()
    {
        StackOutcome::Conflict { index, source, .. } => assert_eq!((index, source), (1, s2)),
        other => panic!("{other:?}"),
    }
}

/// F41-027 (SPEC §7.4): a stack may contain merges. The walk follows first parents; a merge
/// member is rebased by replaying its delta from its first parent and keeps its other
/// parents, so the side branch it merged comes along.
#[tokio::test]
async fn stacks_that_contain_merges_rebase_keeping_their_side_branches() {
    let env = Env::new().await;
    let base = env.commit(&[], &[put("a", "a\n")]).await;
    let side = env.commit(&[base], &[put("side", "from the side\n")]).await;
    let s1 = env.commit(&[base], &[put("a", "a1\n")]).await;
    // The merge of the side branch into the stack, with the merged file in its delta.
    let m = env
        .commit(&[s1, side], &[put("side", "from the side\n")])
        .await;
    let top = env.commit(&[m], &[put("b", "b\n")]).await;
    let main = env.commit(&[base], &[put("z", "z\n")]).await;
    let h = env.history();
    let stack = h.land_stack(&top, &main).await.unwrap();
    assert_eq!(stack, vec![s1, m, top]);
    let StackOutcome::Rebased(steps) = h
        .rebase_stack(&stack, &main, &RebaseOptions::default(), 0)
        .await
        .unwrap()
    else {
        panic!("conflict")
    };
    let merged = h.changeset(&steps[1].result).await.unwrap();
    assert_eq!(
        merged.parents(),
        &[steps[0].result, side],
        "the side branch is kept"
    );
    let want = expect(
        &env.snapshot(&main),
        &[
            put("a", "a1\n"),
            put("side", "from the side\n"),
            put("b", "b\n"),
        ],
    );
    assert_eq!(root_of(&env, &steps[2].result).await, mtree::root_id(&want));
    // The single-change Rebase RPC still takes single-parent changesets only.
    let e = h
        .rebase(&m, &main, &RebaseOptions::default(), 0)
        .await
        .unwrap_err();
    assert!(e.is_invalid(Subcode::InvalidParents));
}

/// F41-019 (SPEC §7.4): with `allow_empty`, a stack member that rebases to nothing writes no
/// changeset; it is recorded with the running tip as its result, and the members after it
/// rebase onto that tip. Without `allow_empty` the same stack is `EMPTY_CHANGE`.
#[tokio::test]
async fn rebase_stack_records_empty_members_at_the_running_tip() {
    let env = Env::new().await;
    let base = env.commit(&[], &[put("a", "a\n")]).await;
    // Someone else already made the first member's change.
    let main = env.commit(&[base], &[put("x", "same\n")]).await;
    let s1 = env.commit(&[base], &[put("x", "same\n")]).await;
    let s2 = env.commit(&[s1], &[put("y", "y\n")]).await;
    let h = env.history();
    let e = h
        .rebase_stack(&[s1, s2], &main, &RebaseOptions::default(), 0)
        .await
        .expect_err("empty");
    assert_eq!(e.reason, Reason::EmptyChange);
    let StackOutcome::Rebased(steps) = h
        .rebase_stack(
            &[s1, s2],
            &main,
            &RebaseOptions {
                allow_empty: true,
                ..RebaseOptions::default()
            },
            0,
        )
        .await
        .unwrap()
    else {
        panic!("conflict")
    };
    assert_eq!(steps[0].source, s1);
    assert_eq!(
        steps[0].result, main,
        "the empty member's result is the tip"
    );
    assert_eq!(
        h.changeset(&steps[1].result).await.unwrap().parents(),
        &[main],
        "the next member rebases onto the tip, not onto an empty changeset"
    );
    assert_eq!(
        file(&env, &steps[1].result, "y").await.as_deref(),
        Some(&b"y\n"[..])
    );
}
