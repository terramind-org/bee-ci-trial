//! Reference commit graph: ancestry, generation numbers, first-parent chains and merge bases,
//! computed by brute force over explicit ancestor sets.

use std::collections::{BTreeMap, BTreeSet};

use bee_core::ObjectId;

/// A commit graph: node to ordered parents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    parents: BTreeMap<ObjectId, Vec<ObjectId>>,
}

impl Graph {
    /// An empty graph.
    pub fn new() -> Graph {
        Graph::default()
    }

    /// Adds a node; its parents should already be present.
    pub fn add(&mut self, id: ObjectId, parents: Vec<ObjectId>) {
        self.parents.insert(id, parents);
    }

    /// True when the node is present.
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.parents.contains_key(id)
    }

    /// Parents of a node.
    pub fn parents(&self, id: &ObjectId) -> &[ObjectId] {
        self.parents.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Children of a node, sorted.
    pub fn children(&self, id: &ObjectId) -> Vec<ObjectId> {
        self.parents
            .iter()
            .filter(|(_, ps)| ps.contains(id))
            .map(|(c, _)| *c)
            .collect()
    }

    /// Every node, sorted.
    pub fn nodes(&self) -> Vec<ObjectId> {
        self.parents.keys().copied().collect()
    }

    /// The node and all its ancestors.
    pub fn ancestors(&self, id: &ObjectId) -> BTreeSet<ObjectId> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![*id];
        while let Some(n) = stack.pop() {
            if seen.insert(n) {
                stack.extend(self.parents(&n).iter().copied());
            }
        }
        seen
    }

    /// True when `a` is `b` or an ancestor of `b`.
    pub fn is_ancestor(&self, a: &ObjectId, b: &ObjectId) -> bool {
        self.ancestors(b).contains(a)
    }

    /// 1 for a parentless node, else one more than the largest parent generation.
    pub fn generation(&self, id: &ObjectId) -> u64 {
        let mut memo = BTreeMap::new();
        self.gen_memo(id, &mut memo)
    }

    fn gen_memo(&self, id: &ObjectId, memo: &mut BTreeMap<ObjectId, u64>) -> u64 {
        if let Some(g) = memo.get(id) {
            return *g;
        }
        let g = 1 + self
            .parents(id)
            .iter()
            .map(|p| self.gen_memo(p, memo))
            .max()
            .unwrap_or(0);
        memo.insert(*id, g);
        g
    }

    /// The node, its first parent, that node's first parent, and so on.
    pub fn first_parent_chain(&self, id: &ObjectId) -> Vec<ObjectId> {
        let mut out = vec![*id];
        while let Some(p) = self.parents(out.last().expect("non-empty")).first() {
            out.push(*p);
        }
        out
    }

    /// The best common ancestors of `a` and `b`: common ancestors that are not ancestors of
    /// another common ancestor. Several means an ambiguous merge base; none means no common
    /// history.
    pub fn merge_bases(&self, a: &ObjectId, b: &ObjectId) -> BTreeSet<ObjectId> {
        let common: BTreeSet<ObjectId> = self
            .ancestors(a)
            .intersection(&self.ancestors(b))
            .copied()
            .collect();
        common
            .iter()
            .filter(|c| {
                !common
                    .iter()
                    .any(|d| d != *c && self.ancestors(d).contains(c))
            })
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;

    fn id(n: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Changeset, [n; 32])
    }

    #[test]
    fn criss_cross_has_two_bases() {
        // 1 <- 2, 1 <- 3, 4 = (2,3), 5 = (3,2), bases of 4 and 5 are {2, 3}.
        let mut g = Graph::new();
        g.add(id(1), vec![]);
        g.add(id(2), vec![id(1)]);
        g.add(id(3), vec![id(1)]);
        g.add(id(4), vec![id(2), id(3)]);
        g.add(id(5), vec![id(3), id(2)]);
        assert_eq!(g.merge_bases(&id(4), &id(5)), [id(2), id(3)].into());
        assert_eq!(g.merge_bases(&id(2), &id(3)), [id(1)].into());
        assert_eq!(g.merge_bases(&id(2), &id(4)), [id(2)].into());
        assert!(g.is_ancestor(&id(1), &id(5)));
        assert!(!g.is_ancestor(&id(4), &id(5)));
        assert_eq!(g.generation(&id(4)), 3);
        assert_eq!(g.first_parent_chain(&id(5)), vec![id(5), id(3), id(1)]);
        assert_eq!(g.children(&id(1)), vec![id(2), id(3)]);
        assert_eq!(g.nodes().len(), 5);
        assert!(g.contains(&id(1)));
        let mut h = Graph::new();
        h.add(id(9), vec![]);
        h.add(id(8), vec![]);
        assert!(h.merge_bases(&id(8), &id(9)).is_empty());
    }
}
