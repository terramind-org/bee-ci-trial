//! The `graph` derived type (SPEC §8) and the ancestry queries built on it.
//!
//! Every changeset gets one row, keyed by its id, type `graph`, version [`GRAPH_VERSION`]:
//!
//! ```text
//! {1: generation, 2: depth, 3: merges, 4: [parent ids], 5: [skip ids]}
//! ```
//!
//! - `generation`: 1 for a parentless changeset, else 1 + the largest parent generation.
//! - `depth`: first-parent steps to a parentless changeset.
//! - `merges`: changesets with two or more parents among this one and its first-parent
//!   ancestors. Two nodes on one first-parent chain with equal counts bound a merge-free span:
//!   every node in it has exactly one parent and generations drop by exactly one per step.
//! - `parents`: the changeset's parents, in order (so graph walks never read changeset rows).
//! - `skips[k]`: the first-parent ancestor at distance 2^k, for every k it exists.
//!
//! [`History::is_ancestor`] and [`History::merge_bases`] walk the DAG in generation order and
//! cross merge-free spans with binary lifting over the skip pointers, so linear history costs
//! O(log n) row reads. Version 1 was the generation-only stand-in written by the test fixtures.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason, Result};
use bee_object::Value;
use bee_store::DerivedRow;

use crate::codec::{self, Fields};
use crate::derive::{Forward, ensure};
use crate::repo::{History, changeset_not_found};

/// The derived type name.
pub const GRAPH_TYPE: &str = "graph";
/// The derived type version.
pub const GRAPH_VERSION: u32 = 2;

/// One changeset's graph row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// Topological level: 1 for a root, else 1 + the largest parent generation.
    pub generation: u64,
    /// First-parent distance to a parentless changeset.
    pub depth: u64,
    /// Merge changesets on the first-parent chain, this one included.
    pub merges: u64,
    /// Parents, in order.
    pub parents: Vec<ObjectId>,
    /// `skips[k]` is the first-parent ancestor at distance 2^k.
    pub skips: Vec<ObjectId>,
}

impl GraphNode {
    /// The node of a parentless changeset.
    pub fn root() -> GraphNode {
        GraphNode {
            generation: 1,
            depth: 0,
            merges: 0,
            parents: Vec::new(),
            skips: Vec::new(),
        }
    }

    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        codec::map([
            (1, Value::Uint(self.generation)),
            (2, Value::Uint(self.depth)),
            (3, Value::Uint(self.merges)),
            (4, codec::ids(&self.parents)),
            (5, codec::ids(&self.skips)),
        ])
        .encode()
    }

    /// Decodes a stored row.
    pub fn decode(bytes: &[u8]) -> Result<GraphNode> {
        let f = Fields::decode(GRAPH_TYPE, bytes)?;
        Ok(GraphNode {
            generation: f.uint(1)?,
            depth: f.uint(2)?,
            merges: f.uint(3)?,
            parents: f.ids(4)?,
            skips: f.ids(5)?,
        })
    }

    /// True when the changeset has two or more parents.
    pub fn is_merge(&self) -> bool {
        self.parents.len() >= 2
    }

    /// The row for changeset `id`.
    pub fn row(&self, id: ObjectId) -> DerivedRow {
        DerivedRow {
            key: id,
            type_name: GRAPH_TYPE.into(),
            version: GRAPH_VERSION,
            value: self.encode(),
        }
    }
}

/// [`Forward`] over graph rows: backfills missing rows from the nearest ancestors that have
/// them (rows written by an older version, or by a writer that did not compute them).
pub struct GraphRows<'a> {
    history: &'a History,
}

impl<'a> GraphRows<'a> {
    /// Graph rows of a repo.
    pub fn new(history: &'a History) -> GraphRows<'a> {
        GraphRows { history }
    }
}

#[async_trait]
impl Forward for GraphRows<'_> {
    fn what(&self) -> &str {
        GRAPH_TYPE
    }

    async fn stored(&self, key: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.history.derived(key, GRAPH_TYPE, GRAPH_VERSION).await
    }

    async fn deps(&self, key: &ObjectId) -> Result<Vec<ObjectId>> {
        Ok(self.history.changeset(key).await?.parents().to_vec())
    }

    async fn derive(&self, _key: &ObjectId, deps: &[(ObjectId, Vec<u8>)]) -> Result<Vec<u8>> {
        let parents: Vec<ObjectId> = deps.iter().map(|(id, _)| *id).collect();
        let nodes: Vec<GraphNode> = deps
            .iter()
            .map(|(_, v)| GraphNode::decode(v))
            .collect::<Result<_>>()?;
        Ok(self
            .history
            .compute_graph_node(&parents, &nodes)
            .await?
            .encode())
    }

    async fn store(&self, key: &ObjectId, value: &[u8]) -> Result<()> {
        self.history
            .put_derived(vec![DerivedRow {
                key: *key,
                type_name: GRAPH_TYPE.into(),
                version: GRAPH_VERSION,
                value: value.to_vec(),
            }])
            .await
    }
}

const LEFT: u8 = 1;
const RIGHT: u8 = 2;
const STALE: u8 = 4;

impl History {
    /// The graph node of a changeset, deriving missing rows forward from the nearest
    /// ancestors that have them. `NOT_FOUND` for an unknown changeset.
    pub async fn graph_node(&self, id: &ObjectId) -> Result<GraphNode> {
        if let Some(n) = self.cached_graph_node(id) {
            return Ok(n);
        }
        let bytes = match self.derived(id, GRAPH_TYPE, GRAPH_VERSION).await? {
            Some(b) => b,
            None => {
                if !self.has_changeset(id).await? {
                    return Err(changeset_not_found(id));
                }
                ensure(&GraphRows::new(self), id).await?
            }
        };
        let node = GraphNode::decode(&bytes)?;
        self.cache_graph_node(*id, node.clone());
        Ok(node)
    }

    /// The graph node a new changeset with these parents gets (the eager row). Parents' rows
    /// are derived first if missing.
    pub async fn graph_node_for(&self, parents: &[ObjectId]) -> Result<GraphNode> {
        let mut nodes = Vec::with_capacity(parents.len());
        for p in parents {
            nodes.push(self.graph_node(p).await?);
        }
        self.compute_graph_node(parents, &nodes).await
    }

    /// Builds a node from its parents' nodes, reading the O(log n) nodes on the skip chain.
    pub(crate) async fn compute_graph_node(
        &self,
        parents: &[ObjectId],
        nodes: &[GraphNode],
    ) -> Result<GraphNode> {
        let (Some(p0), Some(n0)) = (parents.first(), nodes.first()) else {
            return Ok(GraphNode::root());
        };
        let generation = 1 + nodes.iter().map(|n| n.generation).max().unwrap_or(0);
        let merges = n0.merges + u64::from(parents.len() >= 2);
        let mut skips = vec![*p0];
        // skips[k] = skips[k-1] of the node at skips[k-1].
        let mut at = n0.clone();
        loop {
            let k = skips.len() - 1;
            let Some(next) = at.skips.get(k).copied() else {
                break;
            };
            skips.push(next);
            at = self.graph_node(&next).await?;
        }
        Ok(GraphNode {
            generation,
            depth: n0.depth + 1,
            merges,
            parents: parents.to_vec(),
            skips,
        })
    }

    /// The `n`-th first-parent ancestor of `id` (`~n`), in O(log n) row reads; `None` when the
    /// first-parent chain is shorter.
    pub async fn first_parent_ancestor(&self, id: &ObjectId, n: u64) -> Result<Option<ObjectId>> {
        let mut cur = *id;
        let mut node = self.graph_node(id).await?;
        if n > node.depth {
            return Ok(None);
        }
        let mut k = 0usize;
        let mut rest = n;
        while rest > 0 {
            if rest & 1 == 1 {
                let Some(next) = node.skips.get(k).copied() else {
                    return Ok(None);
                };
                cur = next;
                node = self.graph_node(&cur).await?;
                // Re-index from the new node: remaining bits are relative to it.
            }
            rest >>= 1;
            k += 1;
        }
        Ok(Some(cur))
    }

    /// From `x`, the farthest first-parent ancestor in the same merge-free span whose
    /// generation is at least `min_gen` (binary lifting; `x` itself when it cannot move).
    async fn farthest_in_span(
        &self,
        x: ObjectId,
        nx: &GraphNode,
        min_gen: u64,
    ) -> Result<(ObjectId, GraphNode)> {
        let mut z = x;
        let mut nz = nx.clone();
        for k in (0..nx.skips.len()).rev() {
            let Some(s) = nz.skips.get(k).copied() else {
                continue;
            };
            let ns = self.graph_node(&s).await?;
            if ns.merges == nx.merges && ns.generation >= min_gen {
                z = s;
                nz = ns;
            }
        }
        Ok((z, nz))
    }

    /// True when `a` is `b` or an ancestor of `b`. O(log n) row reads when `a` lies on a
    /// merge-free stretch of `b`'s first-parent chain; a generation-ordered walk otherwise.
    pub async fn is_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        let na = self.graph_node(a).await?;
        if a == b {
            return Ok(true);
        }
        let nb = self.graph_node(b).await?;
        if na.generation >= nb.generation {
            return Ok(false);
        }
        let mut heap: BinaryHeap<(u64, ObjectId)> = BinaryHeap::new();
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        heap.push((nb.generation, *b));
        seen.insert(*b);
        while let Some((g, x)) = heap.pop() {
            if x == *a {
                return Ok(true);
            }
            if g <= na.generation {
                continue;
            }
            let nx = self.graph_node(&x).await?;
            let (z, nz) = self.farthest_in_span(x, &nx, na.generation).await?;
            if z == *a {
                return Ok(true);
            }
            if nz.generation <= na.generation {
                continue;
            }
            for p in &nz.parents {
                if seen.insert(*p) {
                    let np = self.graph_node(p).await?;
                    if np.generation >= na.generation {
                        heap.push((np.generation, *p));
                    }
                }
            }
        }
        Ok(false)
    }

    /// The best common ancestors of `a` and `b`, sorted: common ancestors that are not
    /// ancestors of another common ancestor. Empty when the histories are unrelated; several
    /// for a criss-cross.
    pub async fn merge_bases(&self, a: &ObjectId, b: &ObjectId) -> Result<Vec<ObjectId>> {
        self.merge_bases_of(std::slice::from_ref(a), std::slice::from_ref(b))
            .await
    }

    /// The best common ancestors of the union of `lefts`' histories and the union of
    /// `rights`' histories (each id is an ancestor of itself), sorted.
    pub async fn merge_bases_of(
        &self,
        lefts: &[ObjectId],
        rights: &[ObjectId],
    ) -> Result<Vec<ObjectId>> {
        if lefts.is_empty() || rights.is_empty() {
            return Ok(Vec::new());
        }
        let mut walk = Walk::default();
        for (ids, flag) in [(lefts, LEFT), (rights, RIGHT)] {
            for id in ids {
                let n = self.graph_node(id).await?;
                walk.push(*id, n.generation, flag);
            }
        }
        let mut found: BTreeSet<ObjectId> = BTreeSet::new();
        loop {
            if !walk.may_find_more() {
                break;
            }
            let Some((g, x, mut f)) = walk.pop() else {
                break;
            };
            if f & (LEFT | RIGHT) == LEFT | RIGHT && f & STALE == 0 {
                found.insert(x);
                f |= STALE;
            }
            let nx = self.graph_node(&x).await?;
            match walk.peek_gen() {
                Some(ng) if ng < g => {
                    let (z, nz) = self.farthest_in_span(x, &nx, ng).await?;
                    if z != x {
                        walk.push(z, nz.generation, f);
                        continue;
                    }
                }
                Some(ng) if ng == g && !nx.is_merge() && !nx.parents.is_empty() => {
                    if let Some((y, mut fy)) = walk.pop_tie(g) {
                        if fy & (LEFT | RIGHT) == LEFT | RIGHT && fy & STALE == 0 {
                            found.insert(y);
                            fy |= STALE;
                        }
                        let ny = self.graph_node(&y).await?;
                        if !ny.is_merge() && !ny.parents.is_empty() {
                            let floor = walk.peek_gen().unwrap_or(1);
                            let (u, nu, v, nv) = self.lift_pair(x, &nx, y, &ny, floor).await?;
                            if u != x {
                                walk.push(u, nu.generation, f);
                                walk.push(v, nv.generation, fy);
                                continue;
                            }
                        }
                        self.expand(&mut walk, &ny, fy).await?;
                    }
                }
                _ => {}
            }
            self.expand(&mut walk, &nx, f).await?;
        }
        Ok(found.into_iter().collect())
    }

    async fn expand(&self, walk: &mut Walk, node: &GraphNode, flags: u8) -> Result<()> {
        for p in &node.parents {
            let np = self.graph_node(p).await?;
            walk.push(*p, np.generation, flags);
        }
        Ok(())
    }

    /// Lifts two nodes of equal generation down their merge-free spans in step, as far as
    /// they stay distinct and at or above `floor`.
    async fn lift_pair(
        &self,
        x: ObjectId,
        nx: &GraphNode,
        y: ObjectId,
        ny: &GraphNode,
        floor: u64,
    ) -> Result<(ObjectId, GraphNode, ObjectId, GraphNode)> {
        let (mut u, mut nu, mut v, mut nv) = (x, nx.clone(), y, ny.clone());
        for k in (0..nx.skips.len().min(ny.skips.len())).rev() {
            let (Some(su), Some(sv)) = (nu.skips.get(k).copied(), nv.skips.get(k).copied()) else {
                continue;
            };
            if su == sv {
                continue;
            }
            let nsu = self.graph_node(&su).await?;
            if nsu.merges != nx.merges || nsu.generation < floor {
                continue;
            }
            let nsv = self.graph_node(&sv).await?;
            if nsv.merges != ny.merges || nsv.generation < floor {
                continue;
            }
            u = su;
            nu = nsu;
            v = sv;
            nv = nsv;
        }
        Ok((u, nu, v, nv))
    }

    /// The `index`-th parent (1-based, `^n`; 0 is the changeset itself).
    pub async fn nth_parent(&self, id: &ObjectId, index: u32) -> Result<Option<ObjectId>> {
        if index == 0 {
            self.graph_node(id).await?;
            return Ok(Some(*id));
        }
        let node = self.graph_node(id).await?;
        Ok(node.parents.get(index as usize - 1).copied())
    }
}

/// The frontier of a generation-ordered paint walk.
#[derive(Default)]
struct Walk {
    heap: BinaryHeap<(u64, ObjectId)>,
    pending: BTreeMap<ObjectId, u8>,
}

impl Walk {
    fn push(&mut self, id: ObjectId, generation: u64, flags: u8) {
        match self.pending.get_mut(&id) {
            Some(f) => *f |= flags,
            None => {
                self.pending.insert(id, flags);
                self.heap.push((generation, id));
            }
        }
    }

    fn pop(&mut self) -> Option<(u64, ObjectId, u8)> {
        let (g, id) = self.heap.pop()?;
        let f = self.pending.remove(&id).unwrap_or(0);
        Some((g, id, f))
    }

    fn peek_gen(&self) -> Option<u64> {
        self.heap.peek().map(|(g, _)| *g)
    }

    /// Pops the next entry when it is the only other one at generation `g`.
    fn pop_tie(&mut self, g: u64) -> Option<(ObjectId, u8)> {
        let (g1, id) = self.heap.pop()?;
        let third_at_g = self.heap.peek().is_some_and(|(g2, _)| *g2 == g);
        if g1 != g || third_at_g {
            self.heap.push((g1, id));
            return None;
        }
        let f = self.pending.remove(&id).unwrap_or(0);
        Some((id, f))
    }

    /// False once no new best base can appear: every live (non-stale) entry lacks one side.
    fn may_find_more(&self) -> bool {
        let live = self.pending.values().filter(|f| *f & STALE == 0);
        let (mut l, mut r) = (false, false);
        for f in live {
            l |= f & LEFT != 0;
            r |= f & RIGHT != 0;
        }
        l && r
    }
}

/// `NOT_FOUND` for a revision step past the start of history.
pub fn no_such_ancestor(what: &str) -> BeeError {
    BeeError::new(Reason::NotFound, format!("{what} does not exist"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;

    #[test]
    fn node_codec_round_trips() {
        let id = |n: u8| ObjectId::new(ObjectKind::Changeset, [n; 32]);
        let n = GraphNode {
            generation: 9,
            depth: 7,
            merges: 2,
            parents: vec![id(1), id(2)],
            skips: vec![id(1), id(3), id(4)],
        };
        assert_eq!(GraphNode::decode(&n.encode()).unwrap(), n);
        assert!(n.is_merge());
        assert_eq!(
            GraphNode::decode(&GraphNode::root().encode()).unwrap(),
            GraphNode::root()
        );
        assert_eq!(n.row(id(5)).version, GRAPH_VERSION);
        assert!(GraphNode::decode(b"\x01").is_err());
    }

    #[test]
    fn walk_tie_and_liveness() {
        let id = |n: u8| ObjectId::new(ObjectKind::Changeset, [n; 32]);
        let mut w = Walk::default();
        w.push(id(1), 5, LEFT);
        assert!(!w.may_find_more());
        w.push(id(2), 5, RIGHT);
        w.push(id(2), 5, RIGHT);
        assert!(w.may_find_more());
        w.push(id(3), 5, RIGHT | STALE);
        let (g, _, _) = w.pop().unwrap();
        // Two entries left at generation 5: not a lone tie.
        assert!(w.pop_tie(g).is_none());
        assert_eq!(w.heap.len(), 2);
    }
}
