//! Virtual merge bases for criss-cross merges (several best merge bases).

use std::collections::BTreeSet;

use crate::markers::MarkerStyle;
use crate::path::ConflictKind;
use crate::tree::{
    OwnedContent, OwnedSide, OwnedTree, Tree, TreeContent, TreeMergeOptions, Which, borrow_tree,
    merge_trees,
};

/// The history queries a virtual base needs. Implemented by the caller over its commit graph.
pub trait MergeGraph {
    /// A changeset id. Its order decides the fold order, so it must be total and stable.
    type Id: Ord + Clone;

    /// The best common ancestors of the union of `left`'s histories and the union of
    /// `right`'s histories (each id counts as an ancestor of itself). Order does not matter.
    fn merge_bases(&self, left: &[Self::Id], right: &[Self::Id]) -> Vec<Self::Id>;

    /// The flat tree of a changeset.
    fn tree(&self, id: &Self::Id) -> OwnedTree;
}

/// A merge base built by merging several best merge bases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualBase {
    /// The virtual base tree.
    pub tree: OwnedTree,
    /// Paths whose virtual content was produced by resolving a conflict (for text, it holds
    /// conflict markers). A conflict payload built on this base should say so.
    pub conflicted: BTreeSet<Vec<u8>>,
}

impl VirtualBase {
    /// Borrows the virtual tree for [`merge_trees`].
    pub fn as_tree(&self) -> Tree<'_> {
        borrow_tree(&self.tree)
    }
}

/// Recursion limit for nested virtual bases. Deeper (malformed or adversarial) histories use
/// the empty tree as the innermost base.
const MAX_DEPTH: usize = 64;

/// Builds the virtual merge base of `bases` (contract §7).
///
/// No bases gives the empty tree; one gives its tree. Otherwise the bases are sorted by id and
/// folded left: the accumulated virtual tree (ours) is merged with the next base (theirs) over
/// the virtual base of their own merge bases, computed recursively. Conflicts in these inner
/// merges never fail; they resolve into the virtual tree as follows and the path is recorded
/// in [`VirtualBase::conflicted`]:
///
/// - `content`, and `add_add` with a two-way line merge: the marker rendering in
///   [`MarkerStyle::Merge`] (no base section), with ours' mode;
/// - `modify_delete`: the side that is present;
/// - every other kind: ours' side at that path, or absent if ours has none.
pub fn virtual_base<G: MergeGraph>(
    graph: &G,
    bases: &[G::Id],
    opts: &TreeMergeOptions,
) -> VirtualBase {
    build(graph, bases, opts, 0)
}

fn build<G: MergeGraph>(
    graph: &G,
    bases: &[G::Id],
    opts: &TreeMergeOptions,
    depth: usize,
) -> VirtualBase {
    let mut ids: Vec<G::Id> = bases.to_vec();
    ids.sort();
    ids.dedup();
    let Some((first, rest)) = ids.split_first() else {
        return VirtualBase::default();
    };
    let mut acc = VirtualBase {
        tree: graph.tree(first),
        conflicted: BTreeSet::new(),
    };
    let mut folded = vec![first.clone()];
    for next in rest {
        let inner = if depth >= MAX_DEPTH {
            VirtualBase::default()
        } else {
            build(
                graph,
                &graph.merge_bases(&folded, std::slice::from_ref(next)),
                opts,
                depth + 1,
            )
        };
        let next_tree = graph.tree(next);
        acc = merge_step(&inner, &acc, &next_tree, opts);
        folded.push(next.clone());
    }
    acc
}

/// Merges `ours` (the accumulated virtual tree) with `theirs` over `base`, resolving every
/// conflict into the virtual tree.
fn merge_step(
    base: &VirtualBase,
    ours: &VirtualBase,
    theirs: &OwnedTree,
    opts: &TreeMergeOptions,
) -> VirtualBase {
    let base_tree = base.as_tree();
    let ours_tree = ours.as_tree();
    let theirs_tree = borrow_tree(theirs);
    let merged = merge_trees(&base_tree, &ours_tree, &theirs_tree, opts);

    let mut tree = OwnedTree::new();
    let mut conflicted = BTreeSet::new();
    let lookup = |side: Which, path: &[u8]| -> OwnedSide {
        let t = match side {
            Which::Base => &base.tree,
            Which::Ours => &ours.tree,
            Which::Theirs => theirs,
        };
        t[path].clone()
    };
    for (path, entry) in merged.entries {
        let (content, marked) = match entry.content {
            TreeContent::Side { side, path: from } => {
                let marked = match side {
                    Which::Ours => ours.conflicted.contains(&from),
                    Which::Base => base.conflicted.contains(&from),
                    Which::Theirs => false,
                };
                (lookup(side, &from).content, marked)
            }
            TreeContent::Merged(bytes) => {
                // A line merge over marker text from the base or ours carries it on.
                let marked = base.conflicted.contains(&path) || ours.conflicted.contains(&path);
                (OwnedContent::Bytes(bytes), marked)
            }
        };
        if marked {
            conflicted.insert(path.clone());
        }
        tree.insert(
            path,
            OwnedSide {
                mode: entry.mode,
                content,
            },
        );
    }
    for (path, c) in merged.conflicts {
        let resolved = match c.kind {
            ConflictKind::Content | ConflictKind::AddAdd if c.lines.is_some() => {
                let lines = c.lines.as_ref().expect("checked");
                let mode = c.ours.as_ref().or(c.theirs.as_ref()).map(|(_, s)| s.mode);
                mode.map(|mode| OwnedSide {
                    mode,
                    content: OwnedContent::Bytes(lines.render(MarkerStyle::Merge)),
                })
            }
            ConflictKind::ModifyDelete => c
                .ours
                .as_ref()
                .or(c.theirs.as_ref())
                .map(|(_, s)| OwnedSide::from_side(s)),
            _ => c.ours.as_ref().map(|(_, s)| OwnedSide::from_side(s)),
        };
        if let Some(side) = resolved {
            conflicted.insert(path.clone());
            tree.insert(path, side);
        }
    }
    VirtualBase { tree, conflicted }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A toy DAG: id → (parents, tree).
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
            let union = |ids: &[u32]| {
                ids.iter()
                    .flat_map(|&i| self.ancestors(i))
                    .collect::<BTreeSet<u32>>()
            };
            let common: BTreeSet<u32> = union(left).intersection(&union(right)).copied().collect();
            // Best: not an ancestor of another common ancestor.
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

    fn t(entries: &[(&str, &str)]) -> OwnedTree {
        entries
            .iter()
            .map(|(p, c)| {
                (
                    p.as_bytes().to_vec(),
                    OwnedSide {
                        mode: crate::Mode::File,
                        content: OwnedContent::Bytes(c.as_bytes().to_vec()),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn single_and_empty() {
        let dag = Dag {
            nodes: BTreeMap::from([(1, (vec![], t(&[("f", "x\n")])))]),
        };
        assert_eq!(
            virtual_base(&dag, &[], &TreeMergeOptions::default()),
            VirtualBase::default()
        );
        assert_eq!(
            virtual_base(&dag, &[1], &TreeMergeOptions::default()).tree,
            t(&[("f", "x\n")])
        );
    }

    #[test]
    fn conflicting_bases_keep_markers() {
        let dag = Dag {
            nodes: BTreeMap::from([
                (1, (vec![], t(&[("f", "a\n")]))),
                (2, (vec![1], t(&[("f", "b\n")]))),
                (3, (vec![1], t(&[("f", "c\n")]))),
            ]),
        };
        let vb = virtual_base(&dag, &[3, 2], &TreeMergeOptions::default());
        assert_eq!(
            vb.tree,
            t(&[("f", "<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n")])
        );
        assert!(vb.conflicted.contains(&b"f"[..]));
    }
}
