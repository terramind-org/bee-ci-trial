//! The write-ordering protocol (SPEC §6.2) over any [`MetaStore`] and [`BlobStore`].
//!
//! [`Writer`] is the only code that should write objects, changesets and refs. Each step is a
//! named fault point (see [`crate::faults::points`]), so a test can crash between any two
//! steps and check with fsck that nothing dangles:
//!
//! 1. `upload`: verify bytes against ids → write blob-store bytes (non-inline objects) →
//!    insert `objects` rows. A crash after the bytes leaves orphan bytes, which GC removes.
//! 2. `store` (CreateChangeset): upload the new trees (step 1), then one transaction with the
//!    changeset row, parent edges, `object_paths` and eager derived rows. A crash before it
//!    leaves unreferenced object rows; re-running repairs everything.
//!    `store_new` (CreateChangeset with its meta) verifies and writes non-inline bytes as
//!    step 1 does, then commits the object rows and the changeset rows in one
//!    transaction: they become visible and durable together, so nothing dangles either way.
//! 3. `update_ref`: one compare-and-swap transaction. A crash before it leaves a draft.
//!
//! Redaction runs the other way round: the row turns purged first, then the bytes go, so a
//! live row never points at deleted bytes.

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_core::{
    BeeError, ChangeId, Limits, ObjectId, ObjectKind, Reason, RepoPath, Result, Subcode,
};
use bee_object::{Changeset, DeltaEntry, MemoryTrees, Object, Tree};

use crate::blob::BlobStore;
use crate::faults::{FaultInjector, points};
use crate::meta::MetaStore;
use crate::staged::StagedMeta;
use crate::trees::{fetch_path_trees, object_paths_for};
use crate::types::{
    DerivedRow, NewChangeset, NewObject, ObjectPathRow, OrgId, PutOutcome, ReadFrom, Redacted,
    RedactionRow, RefUpdate, RefUpdated, RepoId, StorageClass, UploadRecord,
};

/// A changeset built from a delta, not yet stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltChangeset {
    /// The changeset.
    pub changeset: Changeset,
    /// Trees the build created (root included), keyed by id.
    pub new_trees: BTreeMap<ObjectId, Tree>,
    /// Its `object_paths` rows.
    pub object_paths: Vec<ObjectPathRow>,
}

impl BuiltChangeset {
    /// The changeset id.
    pub fn id(&self) -> ObjectId {
        self.changeset.id()
    }
}

/// Writes through both stores in the SPEC §6.2 order.
#[derive(Clone)]
pub struct Writer {
    meta: Arc<dyn MetaStore>,
    blobs: Arc<dyn BlobStore>,
    faults: Arc<FaultInjector>,
    limits: Limits,
}

impl Writer {
    /// A writer with no fault rules and default limits.
    pub fn new(meta: Arc<dyn MetaStore>, blobs: Arc<dyn BlobStore>) -> Writer {
        Writer {
            meta,
            blobs,
            faults: FaultInjector::new(),
            limits: Limits::DEFAULT,
        }
    }

    /// Uses `faults` for the write-ordering points.
    pub fn with_faults(mut self, faults: Arc<FaultInjector>) -> Writer {
        self.faults = faults;
        self
    }

    /// Uses repo-specific limits when verifying uploads.
    pub fn with_limits(mut self, limits: Limits) -> Writer {
        self.limits = limits;
        self
    }

    /// A writer whose object and changeset rows are buffered in a [`StagedMeta`] over this
    /// writer's metadata store (same blob store, faults and limits): readable through the
    /// returned writer at once, written by [`StagedMeta::flush`] in one transaction.
    pub fn buffered(&self) -> (Writer, Arc<StagedMeta>) {
        let staged = Arc::new(StagedMeta::new(self.meta.clone()));
        let writer = Writer {
            meta: staged.clone(),
            blobs: self.blobs.clone(),
            faults: self.faults.clone(),
            limits: self.limits,
        };
        (writer, staged)
    }

    /// The metadata store.
    pub fn meta(&self) -> &Arc<dyn MetaStore> {
        &self.meta
    }

    /// The blob store.
    pub fn blobs(&self) -> &Arc<dyn BlobStore> {
        &self.blobs
    }

    /// The fault injector.
    pub fn faults(&self) -> &Arc<FaultInjector> {
        &self.faults
    }

    /// Uploads objects (SPEC §6.2 step 1, §7.1): verifies each against its id (`INTEGRITY`,
    /// or `INVALID_ARGUMENT` for non-canonical encodings and changeset or foreign ids),
    /// refuses redacted ids (`REDACTED`), writes blob-store bytes for objects that are not
    /// inlined, then inserts the rows.
    pub async fn upload(
        &self,
        org: OrgId,
        objects: Vec<(ObjectId, Vec<u8>)>,
        upload: Option<UploadRecord>,
        at_ms: u64,
    ) -> Result<Vec<PutOutcome>> {
        let rows = self.prepare_upload(org, objects).await?;
        self.faults
            .guard(
                points::UPLOAD_ROW,
                self.meta.put_objects(org, rows, upload, at_ms),
            )
            .await
    }

    /// The part of [`Writer::upload`] before the rows: verifies the objects, refuses
    /// redacted ids and writes blob-store bytes for objects that are not inlined, returning
    /// the rows to insert.
    async fn prepare_upload(
        &self,
        org: OrgId,
        objects: Vec<(ObjectId, Vec<u8>)>,
    ) -> Result<Vec<NewObject>> {
        let limits = self.limits;
        let checked = self
            .faults
            .guard(points::UPLOAD_VERIFY, async {
                for (id, bytes) in &objects {
                    if !matches!(
                        id.kind(),
                        ObjectKind::Blob | ObjectKind::Tree | ObjectKind::Meta
                    ) {
                        return Err(BeeError::invalid(
                            Subcode::WrongKind,
                            format!("{} objects are not uploaded", id.kind().text_tag()),
                        ));
                    }
                    let obj = Object::verify(id, bytes, &limits)?;
                    if obj.encode() != *bytes {
                        return Err(BeeError::invalid(
                            Subcode::NonCanonical,
                            format!("{id} does not re-encode to its bytes"),
                        ));
                    }
                }
                Ok(())
            })
            .await;
        checked?;
        let ids: Vec<ObjectId> = objects.iter().map(|(id, _)| *id).collect();
        if let Some(r) = self.meta.get_redactions(org, &ids).await?.first() {
            return Err(
                BeeError::new(Reason::Redacted, format!("{} is redacted", r.object))
                    .with_payload("object", r.object.to_text()),
            );
        }
        let mut rows = Vec::with_capacity(objects.len());
        for (id, bytes) in objects {
            let row = NewObject::for_bytes(id, &bytes);
            if row.needs_blob_store() {
                self.faults
                    .guard(
                        points::UPLOAD_BLOB,
                        self.blobs.put(org, &id, bytes, StorageClass::Hot),
                    )
                    .await?;
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// Builds a changeset without writing anything: reads the first parent's root, prefetches
    /// the O(depth) trees the delta touches, applies the delta and computes the
    /// `object_paths` rows. The meta must already be uploaded before [`Writer::store`].
    pub async fn build(
        &self,
        org: OrgId,
        repo: RepoId,
        parents: Vec<ObjectId>,
        meta: ObjectId,
        change_id: ChangeId,
        delta: Vec<DeltaEntry>,
    ) -> Result<BuiltChangeset> {
        // Only the parent's root: its delta (10,000 entries after a bulk import) is not
        // read or decoded, so a build costs the same whatever the parent's size.
        let first_root = match parents.first() {
            Some(p) => Some(
                self.meta
                    .get_changeset_root(repo, p, ReadFrom::Primary)
                    .await?
                    .ok_or_else(|| {
                        BeeError::new(Reason::MissingObject, format!("parent {p} not found"))
                            .with_payload("object", p.to_text())
                    })?,
            ),
            None => None,
        };
        let paths: Vec<RepoPath> = delta.iter().map(|e| e.path.clone()).collect();
        let mut source: MemoryTrees = fetch_path_trees(
            self.meta.as_ref(),
            self.blobs.as_ref(),
            org,
            first_root.as_ref(),
            &paths,
        )
        .await?;
        let (changeset, new_trees) = bee_object::build_changeset(
            &source,
            parents,
            first_root.as_ref(),
            meta,
            change_id,
            delta,
        )?;
        source.extend(new_trees.clone());
        let object_paths = object_paths_for(&source, &changeset)?;
        Ok(BuiltChangeset {
            changeset,
            new_trees,
            object_paths,
        })
    }

    /// Stores a built changeset (SPEC §6.2 step 2): uploads its new trees, then inserts the
    /// changeset with its edges, `object_paths` and the given eager derived rows. Idempotent.
    pub async fn store(
        &self,
        org: OrgId,
        repo: RepoId,
        built: &BuiltChangeset,
        derived: Vec<DerivedRow>,
        at_ms: u64,
    ) -> Result<PutOutcome> {
        let trees: Vec<(ObjectId, Vec<u8>)> = built
            .new_trees
            .iter()
            .map(|(id, t)| (*id, t.encode()))
            .collect();
        if !trees.is_empty() {
            self.upload(org, trees, None, at_ms).await?;
        }
        self.insert_changeset(NewChangeset {
            repo,
            changeset: built.changeset.clone(),
            object_paths: built.object_paths.clone(),
            derived,
            at_ms,
        })
        .await
    }

    /// Stores a new changeset with its meta in one metadata transaction (CreateChangeset,
    /// SPEC §6.2): verifies `meta` and the new trees and writes any non-inline bytes to the
    /// blob store as [`Writer::upload`] does, then inserts their object rows (recording
    /// `upload`) and the changeset row with its edges, `object_paths` and `derived` rows
    /// together ([`MetaStore::insert_changeset_with_objects`]). One durable commit instead
    /// of two; the rows the changeset references commit with it, so the write order
    /// holds. Idempotent.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_new(
        &self,
        org: OrgId,
        repo: RepoId,
        meta: (ObjectId, Vec<u8>),
        built: &BuiltChangeset,
        upload: Option<UploadRecord>,
        derived: Vec<DerivedRow>,
        at_ms: u64,
    ) -> Result<PutOutcome> {
        let mut objects = Vec::with_capacity(built.new_trees.len() + 1);
        objects.push(meta);
        objects.extend(built.new_trees.iter().map(|(id, t)| (*id, t.encode())));
        let rows = self.prepare_upload(org, objects).await?;
        let new = NewChangeset {
            repo,
            changeset: built.changeset.clone(),
            object_paths: built.object_paths.clone(),
            derived,
            at_ms,
        };
        self.faults
            .guard(
                points::CHANGESET_ROW,
                self.meta
                    .insert_changeset_with_objects(org, rows, upload, at_ms, new),
            )
            .await
    }

    /// The changeset transaction alone (every referenced object must already have a row).
    pub async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome> {
        self.faults
            .guard(points::CHANGESET_ROW, self.meta.insert_changeset(new))
            .await
    }

    /// The ref compare-and-swap (SPEC §6.2 step 3).
    pub async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated> {
        self.faults
            .guard(points::REF_CAS, self.meta.update_ref(update))
            .await
    }

    /// Redacts an object (SPEC §13): purges the row and bumps epochs, then deletes the bytes.
    /// Re-running after a crash between the two finishes the job.
    pub async fn redact(&self, row: RedactionRow) -> Result<Redacted> {
        let org = row.org;
        let object = row.object;
        let out = self
            .faults
            .guard(points::REDACT_ROW, self.meta.redact(row))
            .await?;
        self.faults
            .guard(points::REDACT_BYTES, self.blobs.delete(org, &object, None))
            .await?;
        Ok(out)
    }
}
