//! The registry of derived types, and the eager hook that writes eager rows with every
//! changeset.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{ObjectId, Result};
use bee_history::{CoreEager, EagerRows, GRAPH_TYPE, History, PATH_HISTORY_TYPE};
use bee_object::Changeset;
use bee_store::{DerivedRow, EagerType};

use crate::copies::CopyDetectionType;
use crate::core_types::{BlameType, GitMappingType, GraphType, PathHistoryType};
use crate::diff_class::DiffClassType;
use crate::languages::LanguageStatsType;
use crate::types::{DerivedType, Mode};

/// Derived types by name.
#[derive(Clone, Default)]
pub struct Registry {
    types: BTreeMap<&'static str, Arc<dyn DerivedType>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.types
                    .values()
                    .map(|t| (t.name(), t.version(), t.mode())),
            )
            .finish()
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Every built-in type: `graph`, `path_history`, `blame`, `diff_class` (default rules),
    /// `copy_detection`, `language_stats` and `git_mapping`.
    pub fn standard() -> Registry {
        let mut r = Registry::new();
        r.register(Arc::new(GraphType));
        r.register(Arc::new(PathHistoryType));
        r.register(Arc::new(BlameType));
        r.register(Arc::new(DiffClassType::default()));
        r.register(Arc::new(CopyDetectionType));
        r.register(Arc::new(LanguageStatsType));
        r.register(Arc::new(GitMappingType));
        r
    }

    /// Adds a type, replacing any type of the same name (a version bump is a replacement).
    pub fn register(&mut self, t: Arc<dyn DerivedType>) {
        self.types.insert(t.name(), t);
    }

    /// The type named `name`.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn DerivedType>> {
        self.types.get(name)
    }

    /// Every type, by name.
    pub fn types(&self) -> impl Iterator<Item = &Arc<dyn DerivedType>> {
        self.types.values()
    }

    /// The eager types (one changeset-keyed row each), for fsck.
    pub fn eager_types(&self) -> Vec<EagerType> {
        self.types
            .values()
            .filter(|t| t.mode() == Mode::Eager)
            .map(|t| EagerType::new(t.name(), t.version()))
            .collect()
    }
}

#[async_trait]
impl EagerRows for Registry {
    /// `graph` and `path_history` come from `bee-history` (computed together, since path
    /// history needs the generation); every other eager type adds its own rows.
    async fn rows(
        &self,
        history: &History,
        id: ObjectId,
        changeset: &Changeset,
    ) -> Result<Vec<DerivedRow>> {
        let mut rows = CoreEager.rows(history, id, changeset).await?;
        for t in self.types.values() {
            if t.mode() == Mode::Eager && t.name() != GRAPH_TYPE && t.name() != PATH_HISTORY_TYPE {
                rows.extend(t.eager_rows(history, id, changeset).await?);
            }
        }
        Ok(rows)
    }

    fn types(&self) -> Vec<EagerType> {
        self.eager_types()
    }
}
