//! Eager derived rows (SPEC §6.2 step 2, §8): computed for a new changeset and inserted in
//! the same transaction as its row. `object_paths` rows come from
//! [`bee_store::Writer::build`]; this module adds the derived rows and stores the result.

use async_trait::async_trait;
use bee_core::{ChangeId, ObjectId, Result};
use bee_object::{Changeset, DeltaEntry};
use bee_store::{BuiltChangeset, DerivedRow, EagerType};

use crate::graph::{GRAPH_TYPE, GRAPH_VERSION};
use crate::pathhist::{PATH_HISTORY_TYPE, PATH_HISTORY_VERSION};
use crate::repo::History;

/// Produces the eager derived rows of a new changeset.
#[async_trait]
pub trait EagerRows: Send + Sync {
    /// The rows for changeset `id`, which is not stored yet (its parents are).
    async fn rows(
        &self,
        history: &History,
        id: ObjectId,
        changeset: &Changeset,
    ) -> Result<Vec<DerivedRow>>;

    /// The eager types these rows cover, one changeset-keyed row each (for fsck).
    fn types(&self) -> Vec<EagerType>;
}

/// The core eager types: `graph` and `path_history`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreEager;

/// The `(name, version)` pairs [`CoreEager`] writes for every changeset.
pub fn core_eager_types() -> Vec<EagerType> {
    vec![
        EagerType::new(GRAPH_TYPE, GRAPH_VERSION),
        EagerType::new(PATH_HISTORY_TYPE, PATH_HISTORY_VERSION),
    ]
}

#[async_trait]
impl EagerRows for CoreEager {
    async fn rows(
        &self,
        history: &History,
        id: ObjectId,
        changeset: &Changeset,
    ) -> Result<Vec<DerivedRow>> {
        let graph = history.graph_node_for(changeset.parents()).await?;
        let mut rows = history
            .path_history_rows(id, graph.generation, changeset)
            .await?;
        rows.push(graph.row(id));
        Ok(rows)
    }

    fn types(&self) -> Vec<EagerType> {
        core_eager_types()
    }
}

impl History {
    /// Stores a built changeset with its eager rows (SPEC §6.2 step 2). Idempotent.
    pub async fn store_built(&self, built: &BuiltChangeset, at_ms: u64) -> Result<ObjectId> {
        let id = built.id();
        let rows = self.eager().rows(self, id, &built.changeset).await?;
        self.writer()
            .store(self.org(), self.repo(), built, rows, at_ms)
            .await?;
        Ok(id)
    }

    /// CreateChangeset (SPEC §7.2) without authorization: builds trees from the first
    /// parent's root and `delta` (every referenced blob and the meta must already be
    /// uploaded), checks invariant D by construction, and stores the changeset with its
    /// `object_paths` and eager rows. Deterministic and idempotent: the same inputs give the
    /// same id.
    pub async fn create_changeset(
        &self,
        parents: Vec<ObjectId>,
        meta: ObjectId,
        change_id: ChangeId,
        delta: Vec<DeltaEntry>,
        at_ms: u64,
    ) -> Result<ObjectId> {
        let built = self
            .writer()
            .build(self.org(), self.repo(), parents, meta, change_id, delta)
            .await?;
        self.store_built(&built, at_ms).await
    }
}
