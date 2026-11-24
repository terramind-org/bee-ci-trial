//! What a fetch sends: the commits reachable from the wants and not from the client's haves,
//! the shallow boundary, and the trees and blobs those commits introduce, under a filter.
//!
//! - **Commits** are painted in generation order (the `graph` rows' topological level): wants
//!   carry one color, common haves another; a commit popped with only the want color is sent.
//!   A commit's color is final when it is popped, because every child has a higher generation.
//!   The client's shallow commits are grafts: its haves stop there.
//! - **Shallow** requests (`deepen <n>`, `deepen-since`, `deepen-not`, `deepen-relative`)
//!   restrict the want walk to an inside set and report its boundary as `shallow` lines; the
//!   client's shallow commits that end up inside with their parents are `unshallow`ed.
//! - **Trees and blobs** are found by walking each sent commit's tree against the trees of its
//!   parents that the client has or is being sent, path by path: an entry whose id equals the
//!   parent's entry at the same path is skipped with everything under it. An object that moved
//!   between paths may be sent again; never too little.
//! - **Filters**: `blob:none`, `blob:limit=<n>[kmg]` and `tree:<depth>`. Explicitly wanted
//!   objects are always sent (lazy fetches of a partial clone).
//!
//! Everything is a pure function of the request and the repo's immutable objects, which is
//! what makes the pack cacheable by its request digest.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};

use bee_core::{Mode, ObjectId, ObjectKind, Reason};
use bee_git::{DeltaBase, GitOid, ObjectType, PackObject, RawObject, write_pack};
use bee_history::GraphNode;

use crate::error::{GitError, GitResult};
use crate::gitmap::GitView;

/// A partial-clone object filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    /// `blob:none`: no blobs.
    BlobNone,
    /// `blob:limit=<n>`: no blobs of `n` bytes or more.
    BlobLimit(u64),
    /// `tree:<depth>`: no trees or blobs at depth `depth` or deeper (the root tree is at 0).
    TreeDepth(u64),
}

impl Filter {
    /// Parses a filter spec as git sends it.
    pub fn parse(spec: &str) -> GitResult<Filter> {
        let bad = || {
            GitError::invalid(format!(
                "unsupported filter {spec:?}: use blob:none, blob:limit=<n>[kmg] or tree:<depth>"
            ))
        };
        if spec == "blob:none" {
            return Ok(Filter::BlobNone);
        }
        if let Some(n) = spec.strip_prefix("blob:limit=") {
            let (digits, mult) = match n.chars().last() {
                Some('k') | Some('K') => (&n[..n.len() - 1], 1u64 << 10),
                Some('m') | Some('M') => (&n[..n.len() - 1], 1 << 20),
                Some('g') | Some('G') => (&n[..n.len() - 1], 1 << 30),
                _ => (n, 1),
            };
            let v: u64 = digits.parse().map_err(|_| bad())?;
            return Ok(Filter::BlobLimit(v.saturating_mul(mult)));
        }
        if let Some(d) = spec.strip_prefix("tree:") {
            return Ok(Filter::TreeDepth(d.parse().map_err(|_| bad())?));
        }
        Err(bad())
    }

    /// The canonical spelling (for cache keys).
    pub fn spec(&self) -> String {
        match self {
            Filter::BlobNone => "blob:none".into(),
            Filter::BlobLimit(n) => format!("blob:limit={n}"),
            Filter::TreeDepth(d) => format!("tree:{d}"),
        }
    }
}

/// How a shallow fetch limits history.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Deepen {
    /// `deepen <n>`: `n` commits from each want (1 = the wants alone).
    Depth(u32),
    /// `deepen <n>` with `deepen-relative`: `n` commits past the client's shallow boundary.
    Relative(u32),
    /// `deepen-since <seconds>`: commits made at or after this time.
    Since(i64),
    /// `deepen-not <ref>`: commits not reachable from these changesets.
    Not(Vec<ObjectId>),
}

/// A fetch request after negotiation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchArgs {
    /// Objects the client wants.
    pub wants: Vec<GitOid>,
    /// Commits the client says it has.
    pub haves: Vec<GitOid>,
    /// The client's current shallow commits.
    pub shallow: Vec<GitOid>,
    /// History limit.
    pub deepen: Option<Deepen>,
    /// Object filter.
    pub filter: Option<Filter>,
    /// The client accepts offset deltas.
    pub ofs_delta: bool,
    /// The client accepts thin packs: deltas against blobs it already has (those at the same
    /// path in a parent it has), completed on its side. Not used with a filter, since a
    /// partial clone may lack those blobs.
    pub thin_pack: bool,
    /// Send annotated tags that point at sent commits.
    pub include_tag: bool,
}

/// An annotated tag the gateway can serve: its id, bytes and peeled changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnotatedTag {
    /// The tag object's id.
    pub oid: GitOid,
    /// The tag object's bytes.
    pub bytes: Vec<u8>,
    /// The changeset it peels to.
    pub peeled: ObjectId,
}

/// The result of planning a fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchPlan {
    /// Haves the repo has (acknowledged to the client).
    pub common: Vec<GitOid>,
    /// New shallow boundary commits.
    pub shallow: Vec<GitOid>,
    /// Client shallow commits that are no longer shallow.
    pub unshallow: Vec<GitOid>,
    /// The packfile.
    pub pack: Vec<u8>,
    /// Number of objects in the pack.
    pub objects: usize,
}

const WANT: u8 = 1;
const HAVE: u8 = 2;

/// Deltas are tried only for blobs up to this size (delta search is quadratic-ish).
const MAX_DELTA_BYTES: usize = 1 << 20;

struct Graph<'v, 'a> {
    view: &'v mut GitView<'a>,
    nodes: BTreeMap<ObjectId, GraphNode>,
}

impl Graph<'_, '_> {
    async fn node(&mut self, id: &ObjectId) -> GitResult<&GraphNode> {
        if !self.nodes.contains_key(id) {
            let n = self.view.history().graph_node(id).await?;
            self.nodes.insert(*id, n);
        }
        Ok(&self.nodes[id])
    }

    async fn parents(&mut self, id: &ObjectId) -> GitResult<Vec<ObjectId>> {
        Ok(self.node(id).await?.parents.clone())
    }

    async fn generation(&mut self, id: &ObjectId) -> GitResult<u64> {
        Ok(self.node(id).await?.generation)
    }

    /// The commit time used by `deepen-since`: the committer time, else the author time.
    async fn time(&mut self, id: &ObjectId) -> GitResult<i64> {
        let h = self.view.history();
        let cs = h.changeset(id).await?;
        Ok(match h.meta_object(cs.meta()).await? {
            Some(m) => m.committer_time.unwrap_or(m.author_time).seconds,
            None => 0,
        })
    }

    /// Every changeset reachable from `from` (inclusive).
    async fn ancestors(&mut self, from: &[ObjectId]) -> GitResult<BTreeSet<ObjectId>> {
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        let mut stack: Vec<ObjectId> = from.to_vec();
        while let Some(c) = stack.pop() {
            if seen.insert(c) {
                stack.extend(self.parents(&c).await?);
            }
        }
        Ok(seen)
    }
}

/// The inside set and boundary of a shallow request.
struct Shallow {
    inside: BTreeSet<ObjectId>,
    boundary: BTreeSet<ObjectId>,
}

async fn shallow_set(
    g: &mut Graph<'_, '_>,
    wants: &[ObjectId],
    client_shallow: &BTreeSet<ObjectId>,
    deepen: &Deepen,
) -> GitResult<Shallow> {
    let mut inside: BTreeSet<ObjectId> = BTreeSet::new();
    let mut boundary: BTreeSet<ObjectId> = BTreeSet::new();
    match deepen {
        Deepen::Depth(n) | Deepen::Relative(n) => {
            let n = (*n).max(1);
            // Relative depth counts from the client's shallow commits: commits above them
            // (reachable without passing one) are free.
            let relative = matches!(deepen, Deepen::Relative(_));
            let mut queue: VecDeque<(ObjectId, u32)> = VecDeque::new();
            let mut best: BTreeMap<ObjectId, u32> = BTreeMap::new();
            // A 0-1 breadth-first search pops every commit first at its smallest distance.
            for w in wants {
                queue.push_back((*w, if relative { 0 } else { 1 }));
            }
            while let Some((c, d)) = queue.pop_front() {
                if best.contains_key(&c) {
                    continue;
                }
                best.insert(c, d);
                let parents = g.parents(&c).await?;
                if d >= n {
                    if !parents.is_empty() {
                        boundary.insert(c);
                    }
                    continue;
                }
                for p in parents {
                    if relative && d == 0 && !client_shallow.contains(&c) {
                        queue.push_front((p, 0));
                    } else {
                        queue.push_back((p, d + 1));
                    }
                }
            }
            inside.extend(best.keys().copied());
        }
        Deepen::Since(t) => {
            let mut stack: Vec<ObjectId> = wants.to_vec();
            let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
            while let Some(c) = stack.pop() {
                if !seen.insert(c) {
                    continue;
                }
                inside.insert(c);
                for p in g.parents(&c).await? {
                    if g.time(&p).await? >= *t {
                        stack.push(p);
                    } else {
                        boundary.insert(c);
                    }
                }
            }
        }
        Deepen::Not(excluded) => {
            let x = g.ancestors(excluded).await?;
            let mut stack: Vec<ObjectId> =
                wants.iter().filter(|w| !x.contains(w)).copied().collect();
            if stack.is_empty() {
                return Err(GitError::invalid("deepen-not excludes every wanted commit"));
            }
            let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
            while let Some(c) = stack.pop() {
                if !seen.insert(c) {
                    continue;
                }
                inside.insert(c);
                for p in g.parents(&c).await? {
                    if x.contains(&p) {
                        boundary.insert(c);
                    } else {
                        stack.push(p);
                    }
                }
            }
        }
    }
    Ok(Shallow { inside, boundary })
}

/// Plans a fetch: negotiation result, shallow lines and the pack.
pub async fn plan(
    view: &mut GitView<'_>,
    args: &FetchArgs,
    tags: &[AnnotatedTag],
) -> GitResult<FetchPlan> {
    let format = view.format();
    // 1. Wants.
    let mut want_commits: Vec<ObjectId> = Vec::new();
    let mut want_other: Vec<ObjectId> = Vec::new();
    let mut want_tags: Vec<&AnnotatedTag> = Vec::new();
    for w in &args.wants {
        match view.bee_of(w).await? {
            Some(id) if id.kind() == ObjectKind::Changeset => want_commits.push(id),
            Some(id) if matches!(id.kind(), ObjectKind::Tree | ObjectKind::Blob) => {
                want_other.push(id)
            }
            _ => match tags.iter().find(|t| t.oid == *w) {
                Some(t) => {
                    want_tags.push(t);
                    want_commits.push(t.peeled);
                }
                None => {
                    return Err(GitError::new(
                        Reason::NotFound,
                        format!("upload-pack: not our ref {w}"),
                    ));
                }
            },
        }
    }
    want_commits.sort();
    want_commits.dedup();
    // 2. Haves the repo knows.
    let mut common: Vec<GitOid> = Vec::new();
    let mut have_ids: Vec<ObjectId> = Vec::new();
    for h in &args.haves {
        if let Some(id) = view.bee_of(h).await?
            && id.kind() == ObjectKind::Changeset
            && view.history().has_changeset(&id).await?
        {
            if !common.contains(h) {
                common.push(*h);
            }
            have_ids.push(id);
        }
    }
    // 3. The client's shallow commits.
    let mut client_shallow: BTreeSet<ObjectId> = BTreeSet::new();
    let mut shallow_oid: BTreeMap<ObjectId, GitOid> = BTreeMap::new();
    for s in &args.shallow {
        if let Some(id) = view.bee_of(s).await?
            && id.kind() == ObjectKind::Changeset
        {
            client_shallow.insert(id);
            shallow_oid.insert(id, *s);
        }
    }
    let mut g = Graph {
        view,
        nodes: BTreeMap::new(),
    };
    // 4. Shallow limits.
    let limits = match &args.deepen {
        Some(d) => Some(shallow_set(&mut g, &want_commits, &client_shallow, d).await?),
        None => None,
    };
    // The client's shallow commits a deepen request takes inside with their parents: those
    // parents are wanted (the client has the commit, not its history).
    let mut unshallowed: Vec<ObjectId> = Vec::new();
    if let Some(l) = &limits {
        for s in &client_shallow {
            if l.inside.contains(s) && !l.boundary.contains(s) {
                unshallowed.push(*s);
            }
        }
    }
    // 5. Paint.
    let mut colors: BTreeMap<ObjectId, u8> = BTreeMap::new();
    let mut heap: BinaryHeap<(u64, ObjectId)> = BinaryHeap::new();
    let mut queued: BTreeSet<ObjectId> = BTreeSet::new();
    let mut wanting = 0usize; // queued entries without the have color
    let mut send: Vec<ObjectId> = Vec::new();
    let mut seeds: Vec<(ObjectId, u8)> = want_commits.iter().map(|w| (*w, WANT)).collect();
    for s in &unshallowed {
        for p in g.parents(s).await? {
            if limits.as_ref().is_some_and(|l| l.inside.contains(&p)) {
                seeds.push((p, WANT));
            }
        }
    }
    seeds.extend(have_ids.iter().map(|h| (*h, HAVE)));
    for (id, color) in seeds {
        let old = colors.get(&id).copied().unwrap_or(0);
        let new = old | color;
        colors.insert(id, new);
        if queued.insert(id) {
            if new & HAVE == 0 {
                wanting += 1;
            }
            heap.push((g.generation(&id).await?, id));
        } else if old & HAVE == 0 && new & HAVE != 0 {
            wanting -= 1;
        }
    }
    while wanting > 0 {
        let Some((_, id)) = heap.pop() else { break };
        queued.remove(&id);
        let c = colors[&id];
        let (paint, follow) = if c & HAVE != 0 {
            (HAVE, !client_shallow.contains(&id))
        } else {
            wanting -= 1;
            send.push(id);
            let follow = match &limits {
                Some(l) => !l.boundary.contains(&id),
                None => !client_shallow.contains(&id),
            };
            (WANT, follow)
        };
        if !follow {
            continue;
        }
        for p in g.parents(&id).await? {
            if paint == WANT
                && let Some(l) = &limits
                && !l.inside.contains(&p)
            {
                continue;
            }
            let old = colors.get(&p).copied().unwrap_or(0);
            let new = old | paint;
            if new == old {
                continue;
            }
            colors.insert(p, new);
            if queued.insert(p) {
                if new & HAVE == 0 {
                    wanting += 1;
                }
                heap.push((g.generation(&p).await?, p));
            } else if old & HAVE == 0 && new & HAVE != 0 {
                wanting -= 1;
            }
        }
    }
    let sent: BTreeSet<ObjectId> = send.iter().copied().collect();
    let has = |id: &ObjectId| colors.get(id).is_some_and(|c| c & HAVE != 0);
    // 6. Shallow lines.
    let mut shallow = Vec::new();
    let mut unshallow = Vec::new();
    if let Some(l) = &limits {
        for b in &l.boundary {
            if sent.contains(b) && !client_shallow.contains(b) {
                shallow.push(g.view.commit_oid(b).await?);
            }
        }
        for s in &unshallowed {
            unshallow.push(shallow_oid[s]);
        }
    }
    // 7. Trees and blobs.
    let mut walk = ObjectWalk::new(args.filter);
    // Ancestors first: an entry equal to a parent's is skipped on the promise that the parent's
    // walk sends it, so the parent has to be walked before the child. In `send` order (children
    // first) a history that returns to an earlier tree (A, B, then A again) made each tree wait
    // for the other, and the objects only they shared were never sent.
    for c in send.iter().rev() {
        let cs = g.view.history().changeset(c).await?;
        // A parent's objects are on the client (it has the parent) or in this pack.
        let mut bases = Vec::new();
        for p in cs.parents() {
            if sent.contains(p) || has(p) {
                let root = *g.view.history().changeset(p).await?.root();
                bases.push((root, !sent.contains(p)));
            }
        }
        walk.tree(g.view, *cs.root(), bases, false).await?;
    }
    for id in &want_other {
        match id.kind() {
            ObjectKind::Tree => walk.tree(g.view, *id, Vec::new(), true).await?,
            _ => walk.blob(*id, None),
        }
    }
    // 8. Objects in pack order: commits, tags, trees, blobs.
    let view = g.view;
    let mut objects: Vec<PackObject> = Vec::new();
    let mut emitted: BTreeSet<GitOid> = BTreeSet::new();
    let mut push = |objects: &mut Vec<PackObject>, oid: GitOid, po: PackObject| {
        if emitted.insert(oid) {
            objects.push(po);
        }
    };
    for c in &send {
        let (oid, raw) = view.commit_object(c).await?;
        view.remember(*c, oid, true);
        push(&mut objects, oid, PackObject::full(raw));
    }
    for t in tags {
        let wanted = want_tags.iter().any(|w| w.oid == t.oid);
        if wanted || (args.include_tag && sent.contains(&t.peeled)) {
            push(
                &mut objects,
                t.oid,
                PackObject::full(RawObject::new(ObjectType::Tag, t.bytes.clone())),
            );
        }
    }
    for t in &walk.trees {
        let obj = view.tree_object(t).await?;
        view.remember(*t, obj.oid, true);
        push(&mut objects, obj.oid, PackObject::full(obj.raw));
        for (oid, raw) in obj.hidden {
            push(&mut objects, oid, PackObject::full(raw));
        }
    }
    let mut blob_oids: BTreeMap<ObjectId, GitOid> = BTreeMap::new();
    for (b, base) in &walk.blobs {
        let bytes = view.blob_bytes(b).await?;
        let oid = format.hash_object(ObjectType::Blob, &bytes)?;
        view.remember(*b, oid, true);
        blob_oids.insert(*b, oid);
        let mut delta_base = None;
        if let Some(base) = base
            && bytes.len() <= MAX_DELTA_BYTES
        {
            if args.ofs_delta
                && let Some(o) = blob_oids.get(&base.id)
            {
                delta_base = Some(DeltaBase::InPack(*o));
            } else if base.external && args.thin_pack && args.filter.is_none() {
                // A thin pack: the client has this blob; a redacted or oversized base is
                // simply not used.
                if let Ok(base_bytes) = view.blob_bytes(&base.id).await
                    && base_bytes.len() <= MAX_DELTA_BYTES
                {
                    let base_oid = format.hash_object(ObjectType::Blob, &base_bytes)?;
                    delta_base = Some(DeltaBase::External(base_oid, base_bytes));
                }
            }
        }
        push(
            &mut objects,
            oid,
            PackObject {
                object: RawObject::new(ObjectType::Blob, bytes),
                delta_base,
            },
        );
    }
    view.flush().await?;
    let count = objects.len();
    let written = write_pack(format, &objects)?;
    Ok(FetchPlan {
        common,
        shallow,
        unshallow,
        pack: written.pack,
        objects: count,
    })
}

/// Trees at one path that a walk compares against, each marked as held by the client (true)
/// or part of this pack (false).
type Bases = Vec<(ObjectId, bool)>;

/// A blob a delta may be taken against.
#[derive(Clone, Copy, Debug)]
struct BlobBase {
    id: ObjectId,
    /// The base is on the client (a thin pack), not in this pack.
    external: bool,
}

/// The tree and blob part of a fetch.
struct ObjectWalk {
    filter: Option<Filter>,
    trees: Vec<ObjectId>,
    tree_set: BTreeSet<ObjectId>,
    /// Blobs in order, each with a delta base candidate (the blob at the same path in a base).
    blobs: Vec<(ObjectId, Option<BlobBase>)>,
    blob_set: BTreeSet<ObjectId>,
}

impl ObjectWalk {
    fn new(filter: Option<Filter>) -> ObjectWalk {
        ObjectWalk {
            filter,
            trees: Vec::new(),
            tree_set: BTreeSet::new(),
            blobs: Vec::new(),
            blob_set: BTreeSet::new(),
        }
    }

    fn tree_allowed(&self, depth: u64) -> bool {
        !matches!(self.filter, Some(Filter::TreeDepth(d)) if depth >= d)
    }

    fn blob_allowed(&self, depth: u64, size: u64) -> bool {
        match self.filter {
            None => true,
            Some(Filter::BlobNone) => false,
            Some(Filter::BlobLimit(n)) => size < n,
            Some(Filter::TreeDepth(d)) => depth < d,
        }
    }

    fn blob(&mut self, id: ObjectId, base: Option<BlobBase>) {
        if self.blob_set.insert(id) {
            self.blobs.push((id, base));
        }
    }

    /// Walks `root` against `bases` (trees at the same path the client has or gets).
    async fn tree(
        &mut self,
        view: &mut GitView<'_>,
        root: ObjectId,
        bases: Bases,
        forced: bool,
    ) -> GitResult<()> {
        let mut stack: Vec<(ObjectId, Bases, u64, bool)> = vec![(root, bases, 0, forced)];
        while let Some((tid, bases, depth, forced)) = stack.pop() {
            if self.tree_set.contains(&tid) || bases.iter().any(|(b, _)| *b == tid) {
                continue;
            }
            if !forced && !self.tree_allowed(depth) {
                continue;
            }
            self.tree_set.insert(tid);
            self.trees.push(tid);
            let tree = view.history().tree(&tid).await?;
            let mut base_trees = Vec::with_capacity(bases.len());
            for (b, external) in &bases {
                base_trees.push((view.history().tree(b).await?, *external));
            }
            let mut children = Vec::new();
            for e in tree.entries() {
                let same: Vec<(&bee_object::TreeEntry, bool)> = base_trees
                    .iter()
                    .filter_map(|(bt, external)| bt.get(&e.name).map(|be| (be, *external)))
                    .collect();
                if same.iter().any(|(be, _)| be.id == e.id) {
                    continue;
                }
                match e.mode {
                    Mode::Tree => {
                        let sub_bases = same
                            .iter()
                            .filter(|(be, _)| be.mode == Mode::Tree)
                            .map(|(be, external)| (be.id, *external))
                            .collect();
                        children.push((e.id, sub_bases, depth + 1, false));
                    }
                    Mode::Gitlink => {}
                    _ => {
                        if self.blob_set.contains(&e.id) || !self.blob_allowed(depth + 1, e.size) {
                            continue;
                        }
                        // Prefer a base in this pack (an offset delta) over one the client has.
                        let leaves = || same.iter().filter(|(be, _)| be.is_leaf());
                        let base = leaves()
                            .find(|(_, external)| !external)
                            .or_else(|| leaves().next())
                            .map(|(be, external)| BlobBase {
                                id: be.id,
                                external: *external,
                            });
                        self.blob(e.id, base);
                    }
                }
            }
            stack.extend(children.into_iter().rev());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_parse() {
        assert_eq!(Filter::parse("blob:none").unwrap(), Filter::BlobNone);
        assert_eq!(
            Filter::parse("blob:limit=2k").unwrap(),
            Filter::BlobLimit(2048)
        );
        assert_eq!(Filter::parse("blob:limit=7").unwrap(), Filter::BlobLimit(7));
        assert_eq!(Filter::parse("tree:0").unwrap(), Filter::TreeDepth(0));
        assert!(Filter::parse("sparse:oid=abc").is_err());
        assert!(Filter::parse("blob:limit=x").is_err());
        assert_eq!(
            Filter::parse("blob:limit=1m").unwrap().spec(),
            "blob:limit=1048576"
        );
    }

    #[test]
    fn filter_rules() {
        let w = ObjectWalk::new(Some(Filter::TreeDepth(1)));
        assert!(w.tree_allowed(0));
        assert!(!w.tree_allowed(1));
        assert!(!w.blob_allowed(1, 0));
        let w = ObjectWalk::new(Some(Filter::BlobLimit(10)));
        assert!(w.blob_allowed(3, 9));
        assert!(!w.blob_allowed(3, 10));
        let w = ObjectWalk::new(Some(Filter::TreeDepth(0)));
        assert!(!w.tree_allowed(0));
    }
}
