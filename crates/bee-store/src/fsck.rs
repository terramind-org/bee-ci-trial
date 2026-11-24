//! fsck: the invariant oracle (SPEC §14.5, PRODUCT §4.3).
//!
//! [`fsck`] reads everything an org stores through the two traits and reports every broken
//! invariant as a [`Violation`] of an enumerated [`ViolationKind`]. It changes nothing.
//! Orphan bytes (blob-store bytes with no row, left by a crash between the two upload steps)
//! are expected and reported in [`FsckStats`], not as violations; GC removes them.

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{Limits, Mode, ObjectId, ObjectKind, Reason, RefName, RepoPath, Result};
use bee_object::{Changeset, DeltaOp, MemoryTrees, Object, Tree, check_invariant_d, hash_id};

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use crate::types::{ChangesetRow, EagerType, ObjectState, OrgId, ReadFrom, RepoId, is_inline};

/// The kinds of violation fsck reports. The string forms are stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViolationKind {
    /// Stored bytes do not hash to their object id.
    DigestMismatch,
    /// Stored bytes of a tree, meta or changeset are not the canonical encoding.
    NonCanonical,
    /// An object row's size disagrees with its bytes.
    SizeMismatch,
    /// A live object row has no servable bytes (a row without durable bytes).
    MissingBytes,
    /// Inline bytes on an object that must not be inline, or none on one that must.
    BadInline,
    /// An `objects` row for a kind that is never stored there (changeset, foreign).
    BadObjectRow,
    /// A purged object still has bytes (inline or in the blob store).
    RedactedBytesPresent,
    /// A purged object has no redaction record.
    UnexplainedPurge,
    /// A redaction record whose object row is not purged.
    RedactionIncomplete,
    /// A changeset row's bytes do not hash to its id.
    ChangesetIdMismatch,
    /// The `parent_edges` rows disagree with the changeset's parents.
    ParentEdgeMismatch,
    /// A parent has no changeset row in the repo.
    MissingParent,
    /// A changeset references an object (root, meta, delta put, subtree) with no row.
    DanglingReference,
    /// The delta is not the exact difference from the first parent (invariant D).
    InvariantD,
    /// A configured eager derived row is missing.
    EagerRowMissing,
    /// An `(object, path)` reachable in a changeset has no `object_paths` row.
    ObjectPathMissing,
    /// A ref or ref-log entry targets a changeset with no row.
    DanglingRef,
    /// Ref-log versions are not exactly `1..=version`.
    RefLogGap,
    /// A ref-log entry's `old` is not the previous entry's `new`.
    RefLogDiscontinuity,
    /// The last ref-log entry's `new` is not the ref's target.
    RefLogTargetMismatch,
    /// A ref-log landed entry has no matching landed row.
    LandedMissing,
    /// A landed row is not recorded by the ref-log entry of its version.
    LandedOrphan,
    /// One ChangeId landed twice on one ref.
    LandedDuplicate,
    /// An event sequence number beyond the allocated counter.
    EventBeyondCounter,
}

impl ViolationKind {
    /// Every kind, in declaration order.
    pub const ALL: [ViolationKind; 24] = [
        ViolationKind::DigestMismatch,
        ViolationKind::NonCanonical,
        ViolationKind::SizeMismatch,
        ViolationKind::MissingBytes,
        ViolationKind::BadInline,
        ViolationKind::BadObjectRow,
        ViolationKind::RedactedBytesPresent,
        ViolationKind::UnexplainedPurge,
        ViolationKind::RedactionIncomplete,
        ViolationKind::ChangesetIdMismatch,
        ViolationKind::ParentEdgeMismatch,
        ViolationKind::MissingParent,
        ViolationKind::DanglingReference,
        ViolationKind::InvariantD,
        ViolationKind::EagerRowMissing,
        ViolationKind::ObjectPathMissing,
        ViolationKind::DanglingRef,
        ViolationKind::RefLogGap,
        ViolationKind::RefLogDiscontinuity,
        ViolationKind::RefLogTargetMismatch,
        ViolationKind::LandedMissing,
        ViolationKind::LandedOrphan,
        ViolationKind::LandedDuplicate,
        ViolationKind::EventBeyondCounter,
    ];

    /// The stable snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            ViolationKind::DigestMismatch => "digest_mismatch",
            ViolationKind::NonCanonical => "non_canonical",
            ViolationKind::SizeMismatch => "size_mismatch",
            ViolationKind::MissingBytes => "missing_bytes",
            ViolationKind::BadInline => "bad_inline",
            ViolationKind::BadObjectRow => "bad_object_row",
            ViolationKind::RedactedBytesPresent => "redacted_bytes_present",
            ViolationKind::UnexplainedPurge => "unexplained_purge",
            ViolationKind::RedactionIncomplete => "redaction_incomplete",
            ViolationKind::ChangesetIdMismatch => "changeset_id_mismatch",
            ViolationKind::ParentEdgeMismatch => "parent_edge_mismatch",
            ViolationKind::MissingParent => "missing_parent",
            ViolationKind::DanglingReference => "dangling_reference",
            ViolationKind::InvariantD => "invariant_d",
            ViolationKind::EagerRowMissing => "eager_row_missing",
            ViolationKind::ObjectPathMissing => "object_path_missing",
            ViolationKind::DanglingRef => "dangling_ref",
            ViolationKind::RefLogGap => "ref_log_gap",
            ViolationKind::RefLogDiscontinuity => "ref_log_discontinuity",
            ViolationKind::RefLogTargetMismatch => "ref_log_target_mismatch",
            ViolationKind::LandedMissing => "landed_missing",
            ViolationKind::LandedOrphan => "landed_orphan",
            ViolationKind::LandedDuplicate => "landed_duplicate",
            ViolationKind::EventBeyondCounter => "event_beyond_counter",
        }
    }
}

/// One broken invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// What is broken.
    pub kind: ViolationKind,
    /// The repo, when the invariant is per repo.
    pub repo: Option<RepoId>,
    /// The object or changeset involved.
    pub object: Option<ObjectId>,
    /// The ref involved.
    pub ref_name: Option<RefName>,
    /// Human-readable detail.
    pub detail: String,
}

/// Counts of what fsck examined.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckStats {
    /// Object rows.
    pub objects: u64,
    /// Changeset rows.
    pub changesets: u64,
    /// Refs.
    pub refs: u64,
    /// Ref-log entries.
    pub ref_log_entries: u64,
    /// Landed rows.
    pub landed: u64,
    /// Blob-store bytes with no live, non-inline row (GC removes them).
    pub orphan_bytes: Vec<ObjectId>,
}

/// The result of [`fsck`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    /// Every violation found.
    pub violations: Vec<Violation>,
    /// What was examined.
    pub stats: FsckStats,
}

impl FsckReport {
    /// True when nothing is broken.
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    /// The distinct kinds found.
    pub fn kinds(&self) -> BTreeSet<ViolationKind> {
        self.violations.iter().map(|v| v.kind).collect()
    }

    /// How many violations of one kind.
    pub fn count(&self, kind: ViolationKind) -> usize {
        self.violations.iter().filter(|v| v.kind == kind).count()
    }
}

/// What fsck checks beyond the fixed invariants.
#[derive(Clone, Debug)]
pub struct FsckOptions {
    /// Derived types that must exist for every changeset.
    pub eager: Vec<EagerType>,
    /// Limits for decoding.
    pub limits: Limits,
    /// Page size for listings.
    pub page: usize,
}

impl Default for FsckOptions {
    fn default() -> Self {
        FsckOptions {
            eager: Vec::new(),
            limits: Limits::DEFAULT,
            page: 1000,
        }
    }
}

struct Ctx {
    report: FsckReport,
}

impl Ctx {
    fn add(
        &mut self,
        kind: ViolationKind,
        repo: Option<RepoId>,
        object: Option<ObjectId>,
        ref_name: Option<&RefName>,
        detail: impl Into<String>,
    ) {
        self.report.violations.push(Violation {
            kind,
            repo,
            object,
            ref_name: ref_name.cloned(),
            detail: detail.into(),
        });
    }
}

/// Checks every invariant of one org's stored data.
pub async fn fsck(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    opts: &FsckOptions,
) -> Result<FsckReport> {
    let mut cx = Ctx {
        report: FsckReport::default(),
    };
    let page = opts.page.max(1);

    // ----- objects -------------------------------------------------------------------
    let mut states: BTreeMap<ObjectId, ObjectState> = BTreeMap::new();
    let mut inline_rows: BTreeSet<ObjectId> = BTreeSet::new();
    let mut trees = MemoryTrees::new();
    let mut dirty_objects = false;
    let mut after = None;
    loop {
        let rows = meta.list_objects(org, after, page).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows {
            cx.report.stats.objects += 1;
            let id = row.id;
            states.insert(id, row.state);
            if row.inline.is_some() {
                inline_rows.insert(id);
            }
            let kind = id.kind();
            if matches!(kind, ObjectKind::Changeset | ObjectKind::Foreign) {
                cx.add(
                    ViolationKind::BadObjectRow,
                    None,
                    Some(id),
                    None,
                    "changesets and foreign objects have no object rows",
                );
                dirty_objects = true;
                continue;
            }
            match row.state {
                ObjectState::Quarantined => dirty_objects = true,
                ObjectState::Purged => {
                    let present = row.inline.is_some() || blobs.stat(org, &id).await?.is_some();
                    if present {
                        cx.add(
                            ViolationKind::RedactedBytesPresent,
                            None,
                            Some(id),
                            None,
                            "purged object still has bytes",
                        );
                    }
                    if meta.get_redactions(org, &[id]).await?.is_empty() {
                        cx.add(
                            ViolationKind::UnexplainedPurge,
                            None,
                            Some(id),
                            None,
                            "purged without a redaction record",
                        );
                    }
                }
                ObjectState::Live => {
                    let should_inline = usize::try_from(row.size).is_ok_and(|s| is_inline(kind, s));
                    let bytes = match row.inline {
                        Some(b) => {
                            if !is_inline(kind, b.len()) {
                                cx.add(
                                    ViolationKind::BadInline,
                                    None,
                                    Some(id),
                                    None,
                                    "inline bytes on an object that is not inlined",
                                );
                            }
                            Some(b)
                        }
                        None => {
                            if should_inline {
                                cx.add(
                                    ViolationKind::BadInline,
                                    None,
                                    Some(id),
                                    None,
                                    "small tree or meta not stored inline",
                                );
                            }
                            match blobs.get(org, &id).await {
                                Ok(b) => b,
                                // Bytes the store itself cannot verify (bit rot, truncation,
                                // a damaged frame) are a digest violation, not an fsck error.
                                Err(e) if e.reason == Reason::Integrity => {
                                    cx.add(
                                        ViolationKind::DigestMismatch,
                                        None,
                                        Some(id),
                                        None,
                                        format!("stored bytes fail verification: {}", e.message),
                                    );
                                    dirty_objects = true;
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    };
                    let Some(bytes) = bytes else {
                        cx.add(
                            ViolationKind::MissingBytes,
                            None,
                            Some(id),
                            None,
                            "live row without durable bytes",
                        );
                        dirty_objects = true;
                        continue;
                    };
                    if bytes.len() as u64 != row.size {
                        cx.add(
                            ViolationKind::SizeMismatch,
                            None,
                            Some(id),
                            None,
                            format!("row says {} bytes, store has {}", row.size, bytes.len()),
                        );
                    }
                    if hash_id(kind, &bytes) != id {
                        cx.add(
                            ViolationKind::DigestMismatch,
                            None,
                            Some(id),
                            None,
                            "bytes do not hash to the id",
                        );
                        dirty_objects = true;
                        continue;
                    }
                    match Object::decode(kind, &bytes, &opts.limits) {
                        Ok(obj) if obj.encode() == bytes => {
                            if let Object::Tree(t) = obj {
                                trees.insert(t);
                            }
                        }
                        Ok(_) | Err(_) => {
                            if kind != ObjectKind::Blob {
                                cx.add(
                                    ViolationKind::NonCanonical,
                                    None,
                                    Some(id),
                                    None,
                                    "bytes are not the canonical encoding",
                                );
                                dirty_objects = true;
                            }
                        }
                    }
                }
            }
        }
    }
    for r in meta.list_redactions(org).await? {
        if let Some(state) = states.get(&r.object)
            && *state != ObjectState::Purged
        {
            cx.add(
                ViolationKind::RedactionIncomplete,
                None,
                Some(r.object),
                None,
                format!("redacted object is {}", state.as_str()),
            );
        }
    }
    let mut after = None;
    loop {
        let listed = blobs.list(org, after, page).await?;
        let Some((last, _)) = listed.last() else {
            break;
        };
        after = Some(*last);
        for (id, _) in listed {
            let live_external =
                states.get(&id) == Some(&ObjectState::Live) && !inline_rows.contains(&id);
            let quarantined = states.get(&id) == Some(&ObjectState::Quarantined);
            let purged = states.get(&id) == Some(&ObjectState::Purged);
            if !live_external && !quarantined && !purged {
                cx.report.stats.orphan_bytes.push(id);
            }
        }
    }

    // ----- repos -----------------------------------------------------------------------
    for repo in meta.list_repos(org).await? {
        check_repo(meta, repo.id, &states, &trees, dirty_objects, opts, &mut cx).await?;
    }
    Ok(cx.report)
}

async fn check_repo(
    meta: &dyn MetaStore,
    repo: RepoId,
    states: &BTreeMap<ObjectId, ObjectState>,
    trees: &MemoryTrees,
    dirty_objects: bool,
    opts: &FsckOptions,
    cx: &mut Ctx,
) -> Result<()> {
    let page = opts.page.max(1);
    let mut rows: BTreeMap<ObjectId, ChangesetRow> = BTreeMap::new();
    let mut after = None;
    loop {
        let batch = meta.list_changesets(repo, after, page).await?;
        let Some(last) = batch.last() else { break };
        after = Some(last.id);
        for r in batch {
            rows.insert(r.id, r);
        }
    }
    let r = Some(repo);
    let mut decoded: BTreeMap<ObjectId, Changeset> = BTreeMap::new();
    for (id, row) in &rows {
        cx.report.stats.changesets += 1;
        if hash_id(ObjectKind::Changeset, &row.bytes) != *id {
            cx.add(
                ViolationKind::ChangesetIdMismatch,
                r,
                Some(*id),
                None,
                "changeset bytes do not hash to the id",
            );
        }
        let cs = match Changeset::decode_with_limits(&row.bytes, &opts.limits) {
            Ok(c) if c.encode() == row.bytes => c,
            _ => {
                cx.add(
                    ViolationKind::NonCanonical,
                    r,
                    Some(*id),
                    None,
                    "changeset bytes are not canonical",
                );
                row.changeset.clone()
            }
        };
        decoded.insert(*id, cs);
    }

    let mut paths_cache: BTreeMap<ObjectId, BTreeSet<Option<RepoPath>>> = BTreeMap::new();
    let mut walked: BTreeSet<(ObjectId, Option<RepoPath>)> = BTreeSet::new();
    for (id, cs) in &decoded {
        let edges = meta.parents(repo, id).await?;
        if edges.as_slice() != cs.parents() {
            cx.add(
                ViolationKind::ParentEdgeMismatch,
                r,
                Some(*id),
                None,
                format!("edges {:?} vs parents {:?}", edges, cs.parents()),
            );
        }
        let mut parents_ok = true;
        for p in cs.parents() {
            if !rows.contains_key(p) {
                parents_ok = false;
                cx.add(
                    ViolationKind::MissingParent,
                    r,
                    Some(*id),
                    None,
                    format!("parent {p} has no row"),
                );
            }
        }
        let mut refs: Vec<ObjectId> = vec![*cs.root(), *cs.meta()];
        refs.extend(cs.delta().iter().filter_map(|e| match e.op {
            DeltaOp::Put { id, .. } if id.kind() != ObjectKind::Foreign => Some(id),
            _ => None,
        }));
        let mut refs_ok = true;
        for o in refs {
            if !states.contains_key(&o) {
                refs_ok = false;
                cx.add(
                    ViolationKind::DanglingReference,
                    r,
                    Some(*id),
                    None,
                    format!("references {o}, which has no row"),
                );
            }
        }
        if parents_ok && refs_ok {
            let first_root = cs
                .first_parent()
                .and_then(|p| decoded.get(p))
                .map(|p| *p.root());
            if let Err(e) = check_invariant_d(trees, first_root.as_ref(), cs) {
                if e.reason == Reason::MissingObject {
                    if !dirty_objects {
                        cx.add(
                            ViolationKind::DanglingReference,
                            r,
                            Some(*id),
                            None,
                            format!("a reachable tree is missing: {}", e.message),
                        );
                    }
                } else {
                    cx.add(ViolationKind::InvariantD, r, Some(*id), None, e.message);
                }
            }
        }
        for t in &opts.eager {
            if meta
                .get_derived(repo, id, &t.name, t.version, ReadFrom::Primary)
                .await?
                .is_none()
            {
                cx.add(
                    ViolationKind::EagerRowMissing,
                    r,
                    Some(*id),
                    None,
                    format!("no {} v{} row", t.name, t.version),
                );
            }
        }
        // object_paths completeness: every (object, path) reachable from the root.
        let mut stack: Vec<(ObjectId, Option<RepoPath>)> = vec![(*cs.root(), None)];
        while let Some((tree_id, path)) = stack.pop() {
            if !walked.insert((tree_id, path.clone())) {
                continue;
            }
            check_path_row(meta, repo, &tree_id, &path, &mut paths_cache, cx).await?;
            let Ok(tree) = bee_object::TreeSource::tree(trees, &tree_id) else {
                continue;
            };
            for e in Tree::entries(&tree) {
                let child = match &path {
                    Some(p) => p.join(&e.name),
                    None => RepoPath::new(e.name.clone()),
                };
                let Ok(child) = child else { continue };
                if e.mode == Mode::Tree {
                    stack.push((e.id, Some(child)));
                } else if walked.insert((e.id, Some(child.clone()))) {
                    check_path_row(meta, repo, &e.id, &Some(child), &mut paths_cache, cx).await?;
                }
            }
        }
    }

    // ----- refs, ref log, landed index ---------------------------------------------------
    let mut log_landed: BTreeSet<(RefName, u64, bee_core::ChangeId, ObjectId, ObjectId)> =
        BTreeSet::new();
    for rf in meta.list_refs(repo, "").await? {
        cx.report.stats.refs += 1;
        let name = &rf.name;
        if let Some(t) = rf.target
            && !rows.contains_key(&t)
        {
            cx.add(
                ViolationKind::DanglingRef,
                r,
                Some(t),
                Some(name),
                "ref target has no changeset row",
            );
        }
        let log = meta.ref_log(repo, name, 0, usize::MAX).await?;
        cx.report.stats.ref_log_entries += log.len() as u64;
        let contiguous = log.len() as u64 == rf.version
            && log
                .iter()
                .enumerate()
                .all(|(i, e)| e.version == i as u64 + 1);
        if !contiguous {
            cx.add(
                ViolationKind::RefLogGap,
                r,
                None,
                Some(name),
                format!(
                    "ref at version {} has log versions {:?}",
                    rf.version,
                    log.iter().map(|e| e.version).collect::<Vec<_>>()
                ),
            );
        }
        let mut prev: Option<ObjectId> = None;
        let mut seen_cids = BTreeSet::new();
        for (i, e) in log.iter().enumerate() {
            if e.old != prev && (i > 0 || e.old.is_some()) {
                cx.add(
                    ViolationKind::RefLogDiscontinuity,
                    r,
                    None,
                    Some(name),
                    format!("version {} old {:?} after {:?}", e.version, e.old, prev),
                );
            }
            prev = e.new;
            if let Some(t) = e.new
                && !rows.contains_key(&t)
            {
                cx.add(
                    ViolationKind::DanglingRef,
                    r,
                    Some(t),
                    Some(name),
                    format!("ref-log version {} targets a missing changeset", e.version),
                );
            }
            for l in &e.landed {
                if !seen_cids.insert(l.change_id) {
                    cx.add(
                        ViolationKind::LandedDuplicate,
                        r,
                        Some(l.source),
                        Some(name),
                        format!("{} landed twice", l.change_id),
                    );
                }
                log_landed.insert((name.clone(), e.version, l.change_id, l.source, l.result));
                let row = meta
                    .get_landed(repo, name, &[l.change_id])
                    .await?
                    .pop()
                    .flatten();
                let matches = row.is_some_and(|row| {
                    row.source == l.source && row.result == l.result && row.version == e.version
                });
                if !matches {
                    cx.add(
                        ViolationKind::LandedMissing,
                        r,
                        Some(l.source),
                        Some(name),
                        format!(
                            "{} at version {} has no matching landed row",
                            l.change_id, e.version
                        ),
                    );
                }
            }
        }
        if !log.is_empty() && prev != rf.target {
            cx.add(
                ViolationKind::RefLogTargetMismatch,
                r,
                None,
                Some(name),
                format!("log ends at {:?}, ref targets {:?}", prev, rf.target),
            );
        }
    }
    for row in meta.list_landed(repo, None).await? {
        cx.report.stats.landed += 1;
        let key = (
            row.ref_name.clone(),
            row.version,
            row.change_id,
            row.source,
            row.result,
        );
        if !log_landed.contains(&key) {
            cx.add(
                ViolationKind::LandedOrphan,
                r,
                Some(row.source),
                Some(&row.ref_name),
                format!(
                    "{} at version {} is not in the ref log",
                    row.change_id, row.version
                ),
            );
        }
    }

    // ----- events --------------------------------------------------------------------------
    let head = meta.event_head(repo).await?;
    let mut from = 0;
    loop {
        let events = meta
            .read_events(repo, from, page, ReadFrom::Primary)
            .await?;
        let Some(last) = events.last() else { break };
        from = last.seq + 1;
        for e in events {
            if e.seq == 0 || e.seq > head.allocated {
                cx.add(
                    ViolationKind::EventBeyondCounter,
                    r,
                    None,
                    None,
                    format!("event {} beyond allocated {}", e.seq, head.allocated),
                );
            }
        }
    }
    Ok(())
}

async fn check_path_row(
    meta: &dyn MetaStore,
    repo: RepoId,
    object: &ObjectId,
    path: &Option<RepoPath>,
    cache: &mut BTreeMap<ObjectId, BTreeSet<Option<RepoPath>>>,
    cx: &mut Ctx,
) -> Result<()> {
    if !cache.contains_key(object) {
        let paths = meta.object_paths(repo, object, ReadFrom::Primary).await?;
        cache.insert(*object, paths.into_iter().collect());
    }
    if !cache.get(object).is_some_and(|s| s.contains(path)) {
        cx.add(
            ViolationKind::ObjectPathMissing,
            Some(repo),
            Some(*object),
            None,
            format!(
                "no object_paths row at {}",
                path.as_ref()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "<root>".into())
            ),
        );
    }
    Ok(())
}
