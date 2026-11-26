//! Garbage collection (SPEC §6.4): mark and sweep, never blocking writes.
//!
//! **Mark.** Roots are every ref target, every changeset a ref log ever pointed at (so
//! `name@version` keeps resolving), drafts used within the repo's retention window (30 days
//! by default), and caller-supplied roots such as open change requests. Everything reachable
//! from a root through parents is kept, with its meta and every tree and leaf of its root
//! tree. Objects whose row was created, or which were uploaded, within the upload window
//! (24 hours) are kept too.
//!
//! **Sweep.** The rest is proposed to [`MetaStore::gc_sweep`], which deletes it in one
//! transaction unless any candidate was touched after the mark began (in which case nothing
//! is deleted and the next run tries again). Rows go first; then bytes are deleted only if
//! they were last written before the mark, so a concurrent re-upload survives. Blob-store
//! bytes with no row (a crash between the upload steps) are deleted once older than the
//! upload window.
//!
//! **LFS objects** are not in any tree: `lfs_object` derived rows (written by the Git gateway when
//! an LFS object is uploaded) name the blob holding the bytes, and each such blob is a root.
//!
//! **Dry run** computes the same plan without recording a mark or deleting anything.

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{Mode, ObjectId, ObjectKind, Result};

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use crate::trees::read_tree;
use crate::types::{
    DAY_MS, GcEpoch, ObjectState, OrgId, ReadFrom, RepoId, SweepOutcome, SweepPlan,
};

/// The derived type whose rows map a Git LFS object id to the bee blob holding its bytes; the
/// blobs they name are garbage-collection roots.
pub const LFS_OBJECT_TYPE: &str = "lfs_object";

/// GC settings.
#[derive(Clone, Debug)]
pub struct GcOptions {
    /// "Now", from the injected clock.
    pub now_ms: u64,
    /// Objects created or uploaded within this window are never collected (24 h).
    pub upload_window_ms: u64,
    /// Overrides every repo's draft retention, in milliseconds.
    pub draft_retention_ms: Option<u64>,
    /// Plan only.
    pub dry_run: bool,
    /// Extra roots (open change requests and the like).
    pub extra_roots: Vec<(RepoId, ObjectId)>,
    /// Page size for listings.
    pub page: usize,
}

impl GcOptions {
    /// Defaults at time `now_ms`: 24-hour upload window, repo retention, not a dry run.
    pub fn at(now_ms: u64) -> GcOptions {
        GcOptions {
            now_ms,
            upload_window_ms: DAY_MS,
            draft_retention_ms: None,
            dry_run: false,
            extra_roots: Vec::new(),
            page: 1000,
        }
    }
}

/// What GC decided to delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcPlan {
    /// The mark (absent in a dry run).
    pub epoch: Option<GcEpoch>,
    /// Rows to delete.
    pub sweep: SweepPlan,
    /// Blob-store bytes with no row, older than the upload window.
    pub orphan_bytes: Vec<ObjectId>,
    /// Changesets kept.
    pub kept_changesets: u64,
    /// Objects kept.
    pub kept_objects: u64,
}

/// What a GC run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcReport {
    /// The plan.
    pub plan: GcPlan,
    /// The sweep's outcome; `None` in a dry run.
    pub outcome: Option<SweepOutcome>,
    /// Ids whose blob-store bytes were deleted.
    pub deleted_bytes: Vec<ObjectId>,
}

impl GcReport {
    /// True when the sweep was deferred by a concurrent write.
    pub fn deferred(&self) -> bool {
        matches!(self.outcome, Some(SweepOutcome::Deferred { .. }))
    }
}

/// Marks: records a GC epoch (unless dry run), then computes what to delete.
pub async fn plan(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    opts: &GcOptions,
) -> Result<GcPlan> {
    let page = opts.page.max(1);
    let epoch = if opts.dry_run {
        None
    } else {
        Some(meta.gc_begin(org, opts.now_ms).await?)
    };
    let mut out = GcPlan {
        epoch,
        ..GcPlan::default()
    };
    let mut live_objects: BTreeSet<ObjectId> = BTreeSet::new();
    let mut walked_trees: BTreeSet<ObjectId> = BTreeSet::new();
    let mut tree_roots: Vec<ObjectId> = Vec::new();

    for repo in meta.list_repos(org).await? {
        let retention = opts
            .draft_retention_ms
            .unwrap_or(u64::from(repo.draft_retention_days) * DAY_MS);
        let cutoff = opts.now_ms.saturating_sub(retention);
        let mut rows = BTreeMap::new();
        let mut after = None;
        loop {
            let batch = meta.list_changesets(repo.id, after, page).await?;
            let Some(last) = batch.last() else { break };
            after = Some(last.id);
            for r in batch {
                rows.insert(r.id, r);
            }
        }
        let mut roots: Vec<ObjectId> = Vec::new();
        for rf in meta.list_refs(repo.id, "").await? {
            roots.extend(rf.target);
            for e in meta.ref_log(repo.id, &rf.name, 0, usize::MAX).await? {
                roots.extend(e.new);
            }
        }
        roots.extend(
            opts.extra_roots
                .iter()
                .filter(|(r, _)| *r == repo.id)
                .map(|(_, id)| *id),
        );
        roots.extend(
            rows.values()
                .filter(|r| r.last_used_ms >= cutoff)
                .map(|r| r.id),
        );
        let mut after = None;
        loop {
            let batch = meta
                .list_derived_by_type(repo.id, LFS_OBJECT_TYPE, after, page)
                .await?;
            let Some(last) = batch.last() else { break };
            after = Some(last.0);
            for (_, _, value) in batch {
                if let Ok(blob) = ObjectId::from_bytes(&value) {
                    live_objects.insert(blob);
                }
            }
        }
        let mut kept: BTreeSet<ObjectId> = BTreeSet::new();
        while let Some(id) = roots.pop() {
            if !kept.insert(id) {
                continue;
            }
            if let Some(row) = rows.get(&id) {
                roots.extend(row.changeset.parents().iter().copied());
            }
        }
        for (id, row) in &rows {
            if kept.contains(id) {
                out.kept_changesets += 1;
                live_objects.insert(*row.changeset.meta());
                tree_roots.push(*row.changeset.root());
            } else {
                out.sweep.changesets.push((repo.id, *id));
            }
        }
    }

    // Young objects and uploads.
    let young = opts.now_ms.saturating_sub(opts.upload_window_ms);
    let mut rows = BTreeMap::new();
    let mut after = None;
    loop {
        let batch = meta.list_objects(org, after, page).await?;
        let Some(last) = batch.last() else { break };
        after = Some(last.id);
        for r in batch {
            rows.insert(r.id, r);
        }
    }
    for r in rows.values() {
        if r.created_at_ms >= young {
            live_objects.insert(r.id);
            if r.id.kind() == ObjectKind::Tree {
                tree_roots.push(r.id);
            }
        }
    }
    for u in meta.list_uploads(org, young).await? {
        live_objects.insert(u.object);
        if u.object.kind() == ObjectKind::Tree {
            tree_roots.push(u.object);
        }
    }

    // Walk trees.
    while let Some(t) = tree_roots.pop() {
        if !walked_trees.insert(t) {
            continue;
        }
        live_objects.insert(t);
        let Ok(tree) = read_tree(meta, blobs, org, &t, ReadFrom::Primary).await else {
            continue;
        };
        for e in tree.entries() {
            if e.mode == Mode::Tree {
                tree_roots.push(e.id);
            } else {
                live_objects.insert(e.id);
            }
        }
    }

    for (id, r) in &rows {
        if live_objects.contains(id) || r.state == ObjectState::Purged {
            out.kept_objects += 1;
        } else {
            out.sweep.objects.push(*id);
        }
    }
    let mut after = None;
    loop {
        let listed = blobs.list(org, after, page).await?;
        let Some((last, _)) = listed.last() else {
            break;
        };
        after = Some(*last);
        for (id, info) in listed {
            if !rows.contains_key(&id) && info.written_at_ms < young {
                out.orphan_bytes.push(id);
            }
        }
    }
    Ok(out)
}

/// Sweeps a plan made by [`plan`] (not a dry run): deletes rows, then bytes.
pub async fn sweep(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    plan: GcPlan,
    opts: &GcOptions,
) -> Result<GcReport> {
    let Some(epoch) = plan.epoch else {
        return Ok(GcReport {
            plan,
            outcome: None,
            deleted_bytes: Vec::new(),
        });
    };
    let outcome = meta.gc_sweep(epoch, plan.sweep.clone()).await?;
    let mut deleted_bytes = Vec::new();
    if let SweepOutcome::Swept { objects, .. } = &outcome {
        for id in objects {
            if blobs.delete(org, id, Some(epoch.at_ms)).await? {
                deleted_bytes.push(*id);
            }
        }
        let young = opts.now_ms.saturating_sub(opts.upload_window_ms);
        for id in &plan.orphan_bytes {
            if meta.get_object(org, id, ReadFrom::Primary).await?.is_none()
                && blobs.delete(org, id, Some(young)).await?
            {
                deleted_bytes.push(*id);
            }
        }
    }
    Ok(GcReport {
        plan,
        outcome: Some(outcome),
        deleted_bytes,
    })
}

/// Plans and (unless `opts.dry_run`) sweeps.
pub async fn run(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    opts: &GcOptions,
) -> Result<GcReport> {
    let p = plan(meta, blobs, org, opts).await?;
    sweep(meta, blobs, org, p, opts).await
}
