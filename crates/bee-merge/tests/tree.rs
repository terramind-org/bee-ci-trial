//! Tree-level merges: per-path rules, renames, copies, structure and virtual bases
//! (contract §5–§7 and the §13 table).

use std::collections::{BTreeMap, BTreeSet};

use bee_merge::{
    ConflictKind, Content, MergeGraph, Mode, OwnedContent, OwnedSide, OwnedTree, RenameOptions,
    Side, Similarity, Tree, TreeContent, TreeMergeOptions, Which, merge_trees, virtual_base,
};

fn tree<'a>(entries: &[(&str, &'a [u8])]) -> Tree<'a> {
    entries
        .iter()
        .map(|(p, c)| (p.as_bytes().to_vec(), Side::file(c)))
        .collect()
}

fn opts() -> TreeMergeOptions {
    TreeMergeOptions::default()
}

fn kinds(m: &bee_merge::TreeMerge<'_>) -> Vec<(String, ConflictKind)> {
    m.conflicts
        .iter()
        .map(|(p, c)| (String::from_utf8(p.clone()).unwrap(), c.kind))
        .collect()
}

fn p(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

#[test]
fn p1_add_add_has_two_way_hunks() {
    let m = merge_trees(
        &tree(&[]),
        &tree(&[("f", b"a\n")]),
        &tree(&[("f", b"b\n")]),
        &opts(),
    );
    let c = &m.conflicts[&p("f")];
    assert_eq!(c.kind, ConflictKind::AddAdd);
    assert_eq!(c.hunks.len(), 1);
    let h = c.hunks[0];
    assert_eq!(
        (
            h.base_start,
            h.base_len,
            h.ours_start,
            h.ours_len,
            h.theirs_start,
            h.theirs_len
        ),
        (0, 0, 0, 1, 0, 1)
    );
}

#[test]
fn identical_independent_adds_merge() {
    let m = merge_trees(
        &tree(&[]),
        &tree(&[("f", b"same\n")]),
        &tree(&[("f", b"same\n")]),
        &opts(),
    );
    assert!(m.is_clean());
    assert_eq!(
        m.entries[&p("f")].content,
        TreeContent::Side {
            side: Which::Ours,
            path: p("f")
        }
    );
}

#[test]
fn p2_modify_delete() {
    let m = merge_trees(
        &tree(&[("f", b"a\n")]),
        &tree(&[]),
        &tree(&[("f", b"b\n")]),
        &opts(),
    );
    assert_eq!(kinds(&m), vec![("f".into(), ConflictKind::ModifyDelete)]);
}

#[test]
fn p3_to_p6_modes_symlinks_binary_redaction() {
    let base: Tree<'_> = [(p("f"), Side::file(b"a\n"))].into();
    let exec: Tree<'_> = [(p("f"), Side::new(Mode::Executable, b"a\n"))].into();
    let changed: Tree<'_> = [(p("f"), Side::file(b"b\n"))].into();
    let m = merge_trees(&base, &exec, &changed, &opts());
    assert!(m.is_clean());
    assert_eq!(m.entries[&p("f")].mode, Mode::Executable);
    assert_eq!(
        m.entries[&p("f")].content,
        TreeContent::Side {
            side: Which::Theirs,
            path: p("f")
        }
    );

    let link: Tree<'_> = [(p("f"), Side::new(Mode::Symlink, b"a\n"))].into();
    assert_eq!(
        kinds(&merge_trees(&base, &exec, &link, &opts())),
        vec![("f".into(), ConflictKind::Symlink)]
    );

    let binary: Tree<'_> = [(p("f"), Side::file(b"\0bin"))].into();
    assert_eq!(
        kinds(&merge_trees(&base, &binary, &changed, &opts())),
        vec![("f".into(), ConflictKind::Binary)]
    );

    let red = |mode| Side {
        mode,
        content: Content::Redacted(b"blob-id"),
    };
    let rbase: Tree<'_> = [(p("f"), red(Mode::File))].into();
    let rexec: Tree<'_> = [(p("f"), red(Mode::Executable))].into();
    let m = merge_trees(&rbase, &rexec, &changed, &opts());
    assert!(m.is_clean());
    assert_eq!(m.entries[&p("f")].mode, Mode::Executable);
    // Both sides change a redacted file's content: the bytes are needed.
    let other: Tree<'_> = [(p("f"), Side::file(b"c\n"))].into();
    assert_eq!(
        kinds(&merge_trees(&rbase, &other, &changed, &opts())),
        vec![("f".into(), ConflictKind::Redacted)]
    );
}

#[test]
fn t1_change_follows_exact_rename() {
    let base = tree(&[("a.txt", b"1\n2\n3\n4\n5\n")]);
    let ours = tree(&[("b.txt", b"1\n2\n3\n4\n5\n")]);
    let theirs = tree(&[("a.txt", b"1\n2\n3\n4\nFIVE\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert!(m.is_clean());
    assert_eq!(
        m.entries.keys().cloned().collect::<Vec<_>>(),
        vec![p("b.txt")]
    );
    assert_eq!(
        m.entries[&p("b.txt")].content,
        TreeContent::Side {
            side: Which::Theirs,
            path: p("a.txt")
        }
    );
    assert_eq!(m.renames.len(), 1);
    assert_eq!(m.renames[0].similarity, Similarity::Exact);
}

#[test]
fn edits_on_both_sides_merge_across_a_similar_rename() {
    // AS-MERGE-02 shape: one agent renames and edits the top; another edits the bottom.
    let base = tree(&[(
        "src/lib.rs",
        b"use a;\nfn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n",
    )]);
    let ours = tree(&[(
        "src/core.rs",
        b"use b;\nfn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n",
    )]);
    let theirs = tree(&[(
        "src/lib.rs",
        b"use a;\nfn one() {}\nfn two() {}\nfn three() {}\nfn FOUR() {}\n",
    )]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert!(m.is_clean(), "{:?}", kinds(&m));
    assert_eq!(
        m.entries[&p("src/core.rs")].content,
        TreeContent::Merged(
            b"use b;\nfn one() {}\nfn two() {}\nfn three() {}\nfn FOUR() {}\n".to_vec()
        )
    );
    assert!(matches!(
        m.renames[0].similarity,
        Similarity::Lines {
            shared: 4,
            union: 6
        }
    ));
    // With rename detection off the same inputs conflict.
    let off = TreeMergeOptions {
        renames: RenameOptions {
            enabled: false,
            ..RenameOptions::default()
        },
        ..opts()
    };
    let m = merge_trees(&base, &ours, &theirs, &off);
    assert_eq!(
        kinds(&m),
        vec![("src/lib.rs".into(), ConflictKind::ModifyDelete)]
    );
}

#[test]
fn both_sides_rename_to_the_same_path() {
    let base = tree(&[("a", b"1\n2\n3\n")]);
    let ours = tree(&[("b", b"0\n1\n2\n3\n")]);
    let theirs = tree(&[("b", b"1\n2\n3\n4\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert!(m.is_clean());
    assert_eq!(
        m.entries[&p("b")].content,
        TreeContent::Merged(b"0\n1\n2\n3\n4\n".to_vec())
    );
}

#[test]
fn t2_rename_rename_conflicts_at_both_destinations() {
    let base = tree(&[("a", b"x\n")]);
    let m = merge_trees(
        &base,
        &tree(&[("b", b"x\n")]),
        &tree(&[("c", b"x\n")]),
        &opts(),
    );
    assert_eq!(
        kinds(&m),
        vec![
            ("b".into(), ConflictKind::RenameRename),
            ("c".into(), ConflictKind::RenameRename)
        ]
    );
    let c = &m.conflicts[&p("b")];
    assert_eq!(c.base.as_ref().unwrap().0, p("a"));
    assert_eq!(c.ours.as_ref().unwrap().0, p("b"));
    assert_eq!(c.theirs.as_ref().unwrap().0, p("c"));
}

#[test]
fn t3_rename_against_delete() {
    let m = merge_trees(
        &tree(&[("a", b"x\n")]),
        &tree(&[("b", b"x\n")]),
        &tree(&[]),
        &opts(),
    );
    assert_eq!(kinds(&m), vec![("b".into(), ConflictKind::ModifyDelete)]);
}

#[test]
fn rename_onto_an_occupied_path_is_not_followed() {
    // Theirs independently created `b`; ours renamed `a` to `b`. Nothing is lost.
    let base = tree(&[("a", b"x\n")]);
    let ours = tree(&[("b", b"x\n")]);
    let theirs = tree(&[("a", b"x\nmore\n"), ("b", b"theirs\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert_eq!(
        kinds(&m),
        vec![
            ("a".into(), ConflictKind::ModifyDelete),
            ("b".into(), ConflictKind::AddAdd)
        ]
    );
    assert!(m.renames.is_empty());
}

#[test]
fn t4_file_directory() {
    let base = tree(&[("d/x", b"x\n")]);
    let ours = tree(&[("d", b"file\n")]);
    let theirs = tree(&[("d/x", b"x\n"), ("d/y", b"y\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert_eq!(
        kinds(&m),
        vec![
            ("d".into(), ConflictKind::FileDirectory),
            ("d/y".into(), ConflictKind::FileDirectory)
        ]
    );
    // Replacing the directory with a file is clean when the other side left the directory alone.
    let m = merge_trees(&base, &ours, &base, &opts());
    assert!(m.is_clean());
    assert_eq!(m.entries.keys().cloned().collect::<Vec<_>>(), vec![p("d")]);
}

#[test]
fn file_directory_overrides_an_existing_conflict_kind() {
    let base = tree(&[("d/x", b"x\n")]);
    let ours = tree(&[("d", b"file\n")]);
    let theirs = tree(&[("d/x", b"changed\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert_eq!(
        kinds(&m),
        vec![
            ("d".into(), ConflictKind::FileDirectory),
            ("d/x".into(), ConflictKind::FileDirectory)
        ]
    );
}

#[test]
fn t5_copies_from_the_same_source_merge_three_way() {
    let src: &[u8] = b"1\n2\n3\n4\n5\n6\n";
    let base = tree(&[("src", src)]);
    let ours = tree(&[("src", src), ("cp", b"0\n1\n2\n3\n4\n5\n6\n")]);
    let theirs = tree(&[("src", src), ("cp", b"1\n2\n3\n4\n5\n6\n7\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    assert!(m.is_clean(), "{:?}", kinds(&m));
    assert_eq!(
        m.entries[&p("cp")].content,
        TreeContent::Merged(b"0\n1\n2\n3\n4\n5\n6\n7\n".to_vec())
    );
    // Conflicting edits of the copies conflict as content, with the copy source as base.
    let ours = tree(&[("src", src), ("cp", b"1\nX\n3\n4\n5\n6\n")]);
    let theirs = tree(&[("src", src), ("cp", b"1\nY\n3\n4\n5\n6\n")]);
    let m = merge_trees(&base, &ours, &theirs, &opts());
    let c = &m.conflicts[&p("cp")];
    assert_eq!(c.kind, ConflictKind::Content);
    assert_eq!(c.base.as_ref().unwrap().0, p("src"));
}

#[test]
fn tree_merge_is_deterministic() {
    let base = tree(&[("a", b"1\n2\n3\n"), ("b", b"x\ny\n"), ("c/d", b"q\n")]);
    let ours = tree(&[
        ("a2", b"1\n2\n3\n"),
        ("b", b"x\nY\n"),
        ("c/d", b"q\n"),
        ("n", b"new\n"),
    ]);
    let theirs = tree(&[("a", b"1\n2\n3\n4\n"), ("b", b"X\ny\n"), ("c", b"file\n")]);
    let first = merge_trees(&base, &ours, &theirs, &opts());
    for _ in 0..5 {
        assert_eq!(merge_trees(&base, &ours, &theirs, &opts()), first);
    }
}

/// A toy commit graph for virtual bases.
struct Dag {
    nodes: BTreeMap<u32, (Vec<u32>, OwnedTree)>,
}

impl Dag {
    fn ancestors(&self, id: u32) -> BTreeSet<u32> {
        let mut out = BTreeSet::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if out.insert(n) {
                stack.extend(self.nodes[&n].0.iter().copied());
            }
        }
        out
    }
}

impl MergeGraph for Dag {
    type Id = u32;

    fn merge_bases(&self, left: &[u32], right: &[u32]) -> Vec<u32> {
        let all = |ids: &[u32]| {
            ids.iter()
                .flat_map(|&i| self.ancestors(i))
                .collect::<BTreeSet<_>>()
        };
        let common: BTreeSet<u32> = all(left).intersection(&all(right)).copied().collect();
        common
            .iter()
            .copied()
            .filter(|&c| {
                !common
                    .iter()
                    .any(|&o| o != c && self.ancestors(o).contains(&c))
            })
            .collect()
    }

    fn tree(&self, id: &u32) -> OwnedTree {
        self.nodes[id].1.clone()
    }
}

fn owned(entries: &[(&str, &str)]) -> OwnedTree {
    entries
        .iter()
        .map(|(p, c)| {
            (
                p.as_bytes().to_vec(),
                OwnedSide {
                    mode: Mode::File,
                    content: OwnedContent::Bytes(c.as_bytes().to_vec()),
                },
            )
        })
        .collect()
}

#[test]
fn v1_criss_cross_merge_uses_a_virtual_base() {
    // 1 is the root. 2 and 3 branch from it; 4 = merge(2, 3) and 5 = merge(3, 2) both exist,
    // so 4 and 5 have two best merge bases: 2 and 3.
    let root = "a\nb\nc\nd\ne\n";
    let two = "A\nb\nc\nd\ne\n";
    let three = "a\nb\nc\nd\nE\n";
    let merged = "A\nb\nc\nd\nE\n";
    let dag = Dag {
        nodes: BTreeMap::from([
            (1, (vec![], owned(&[("f", root)]))),
            (2, (vec![1], owned(&[("f", two)]))),
            (3, (vec![1], owned(&[("f", three)]))),
            (4, (vec![2, 3], owned(&[("f", "A\nb\nC\nd\nE\n")]))),
            (5, (vec![3, 2], owned(&[("f", merged), ("g", "new\n")]))),
        ]),
    };
    let bases = dag.merge_bases(&[4], &[5]);
    assert_eq!(bases, vec![2, 3]);
    let vb = virtual_base(&dag, &bases, &opts());
    // The virtual base is the clean merge of 2 and 3 over their base 1.
    assert_eq!(vb.tree, owned(&[("f", merged)]));
    assert!(vb.conflicted.is_empty());
    let ours = dag.tree(&4);
    let theirs = dag.tree(&5);
    let (o, t) = (
        bee_merge::borrow_tree(&ours),
        bee_merge::borrow_tree(&theirs),
    );
    let m = merge_trees(&vb.as_tree(), &o, &t, &opts());
    assert!(m.is_clean());
    assert_eq!(
        m.entries[&p("f")].content,
        TreeContent::Side {
            side: Which::Ours,
            path: p("f")
        }
    );
    assert_eq!(
        m.entries[&p("g")].content,
        TreeContent::Side {
            side: Which::Theirs,
            path: p("g")
        }
    );
}

#[test]
fn virtual_base_conflicts_are_marked_and_deterministic() {
    let dag = Dag {
        nodes: BTreeMap::from([
            (1, (vec![], owned(&[("f", "a\n"), ("gone", "g\n")]))),
            (2, (vec![1], owned(&[("f", "b\n")]))),
            (3, (vec![1], owned(&[("f", "c\n"), ("gone", "changed\n")]))),
        ]),
    };
    let vb = virtual_base(&dag, &[3, 2], &opts());
    assert_eq!(vb, virtual_base(&dag, &[2, 3], &opts()));
    assert_eq!(
        vb.tree,
        owned(&[
            ("f", "<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n"),
            ("gone", "changed\n")
        ])
    );
    assert_eq!(vb.conflicted, BTreeSet::from([p("f"), p("gone")]));
}

#[test]
fn nested_virtual_bases() {
    // Two criss-crosses stacked: the bases of 6 and 7 are 4 and 5, whose own bases are 2 and 3.
    let dag = Dag {
        nodes: BTreeMap::from([
            (1, (vec![], owned(&[("f", "1\n2\n3\n4\n5\n6\n")]))),
            (2, (vec![1], owned(&[("f", "X\n2\n3\n4\n5\n6\n")]))),
            (3, (vec![1], owned(&[("f", "1\n2\n3\n4\n5\nY\n")]))),
            (4, (vec![2, 3], owned(&[("f", "X\n2\n3\n4\n5\nY\n")]))),
            (
                5,
                (
                    vec![3, 2],
                    owned(&[("f", "X\n2\n3\n4\n5\nY\n"), ("h", "h\n")]),
                ),
            ),
            (
                6,
                (
                    vec![4, 5],
                    owned(&[("f", "X\n2\nZ\n4\n5\nY\n"), ("h", "h\n")]),
                ),
            ),
            (
                7,
                (
                    vec![5, 4],
                    owned(&[("f", "X\n2\n3\n4\nW\nY\n"), ("h", "h\n")]),
                ),
            ),
        ]),
    };
    let bases = dag.merge_bases(&[6], &[7]);
    assert_eq!(bases, vec![4, 5]);
    let vb = virtual_base(&dag, &bases, &opts());
    assert_eq!(vb.tree, owned(&[("f", "X\n2\n3\n4\n5\nY\n"), ("h", "h\n")]));
    let (o, t) = (dag.tree(&6), dag.tree(&7));
    let (o, t) = (bee_merge::borrow_tree(&o), bee_merge::borrow_tree(&t));
    let m = merge_trees(&vb.as_tree(), &o, &t, &opts());
    assert!(m.is_clean());
    assert_eq!(
        m.entries[&p("f")].content,
        TreeContent::Merged(b"X\n2\nZ\n4\nW\nY\n".to_vec())
    );
}
