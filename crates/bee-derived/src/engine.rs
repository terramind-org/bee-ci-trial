//! The generic read API (GetDerived) and lazy derivation with optional leases.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason, Result};
use bee_history::{Forward, GraphNode, History, ensure_with_stats};
use bee_store::DerivedRow;

use crate::copies::{COPY_DETECTION_TYPE, CopyRecord, decode_copies};
use crate::diff_class::{DIFF_CLASS_TYPE, DiffClassValue};
use crate::languages::{LANGUAGE_STATS_TYPE, LanguageStats, decode_stats};
use crate::registry::Registry;
use crate::types::{DerivedType, DerivedValue, Mode};

type LeaseKey = (String, u32, ObjectId);

/// Derivation leases (SPEC §8): concurrent requests for the same value wait for the one
/// already deriving it instead of duplicating the work. Optional — values are written
/// insert-or-ignore, so racing derivations are only wasted effort.
#[derive(Debug, Default)]
pub struct Leases {
    held: Mutex<BTreeMap<LeaseKey, Arc<tokio::sync::Mutex<()>>>>,
}

impl Leases {
    /// No leases held.
    pub fn new() -> Leases {
        Leases::default()
    }

    fn slot(&self, key: &LeaseKey) -> Arc<tokio::sync::Mutex<()>> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.clone())
            .or_default()
            .clone()
    }

    fn release(&self, key: &LeaseKey, slot: &Arc<tokio::sync::Mutex<()>>) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        // The map and the caller hold the only references: nobody else is waiting.
        if Arc::strong_count(slot) <= 2 {
            held.remove(key);
        }
    }

    /// Number of keys currently leased (for tests and metrics).
    pub fn active(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// [`Forward`] over one derived type.
struct TypeRows<'a> {
    t: &'a dyn DerivedType,
    history: &'a History,
}

#[async_trait]
impl Forward for TypeRows<'_> {
    fn what(&self) -> &str {
        self.t.name()
    }
    async fn stored(&self, key: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.history
            .derived(key, self.t.name(), self.t.version())
            .await
    }
    async fn deps(&self, key: &ObjectId) -> Result<Vec<ObjectId>> {
        self.t.deps(self.history, key).await
    }
    async fn derive(&self, key: &ObjectId, deps: &[(ObjectId, Vec<u8>)]) -> Result<Vec<u8>> {
        self.t.derive(self.history, key, deps).await
    }
    async fn store(&self, key: &ObjectId, value: &[u8]) -> Result<()> {
        self.history
            .put_derived(vec![DerivedRow {
                key: *key,
                type_name: self.t.name().into(),
                version: self.t.version(),
                value: value.to_vec(),
            }])
            .await
    }
}

/// The derived-data engine for one repo: generic reads by type name, typed accessors, and
/// the eager hook (the registry) for changesets the history handle creates.
pub struct Derived {
    history: Arc<History>,
    registry: Arc<Registry>,
    leases: Option<Arc<Leases>>,
}

fn unknown_type(name: &str) -> BeeError {
    BeeError::new(Reason::NotFound, format!("unknown derived type {name:?}"))
        .with_payload("type", name)
}

impl Derived {
    /// An engine over `history` with `registry`'s types. Make the history handle write the
    /// registry's eager rows with `History::with_eager(registry.clone())`.
    pub fn new(history: Arc<History>, registry: Arc<Registry>) -> Derived {
        Derived {
            history,
            registry,
            leases: None,
        }
    }

    /// Shares derivation leases between engines (one per process is typical).
    pub fn with_leases(mut self, leases: Arc<Leases>) -> Derived {
        self.leases = Some(leases);
        self
    }

    /// The history handle.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// GetDerived: the current version's value of `type_name` for `key`, deriving it (and
    /// any missing dependencies, forward and iteratively) when it is not stored.
    /// `NOT_FOUND` for an unknown type, an unknown key, or an importer-written type with no
    /// row.
    pub async fn get(&self, type_name: &str, key: &ObjectId) -> Result<DerivedValue> {
        let t = self
            .registry
            .get(type_name)
            .ok_or_else(|| unknown_type(type_name))?
            .clone();
        let storage = t.storage_key(key);
        let value = |bytes: Vec<u8>, derived: bool| DerivedValue {
            type_name: t.name().into(),
            version: t.version(),
            key: *key,
            value: bytes,
            derived,
        };
        if let Some(v) = self
            .history
            .derived(&storage, t.name(), t.version())
            .await?
        {
            return Ok(value(v, false));
        }
        if t.mode() == Mode::Importer {
            return Err(BeeError::new(
                Reason::NotFound,
                format!("no {} row for {key}", t.name()),
            ));
        }
        let lease_key = (t.name().to_string(), t.version(), storage);
        let lease = self
            .leases
            .as_ref()
            .map(|l| (l.clone(), l.slot(&lease_key)));
        let _guard = match &lease {
            Some((_, slot)) => Some(slot.lock().await),
            None => None,
        };
        // Whoever held the lease may have written it meanwhile.
        let result = match self
            .history
            .derived(&storage, t.name(), t.version())
            .await?
        {
            Some(v) => Ok(value(v, false)),
            None => self
                .derive(t.as_ref(), key, &storage)
                .await
                .map(|v| value(v, true)),
        };
        drop(_guard);
        if let Some((leases, slot)) = &lease {
            leases.release(&lease_key, slot);
        }
        result
    }

    async fn derive(
        &self,
        t: &dyn DerivedType,
        key: &ObjectId,
        storage: &ObjectId,
    ) -> Result<Vec<u8>> {
        if t.has_deps() {
            let rows = TypeRows {
                t,
                history: &self.history,
            };
            return ensure_with_stats(&rows, key).await.map(|(v, _)| v);
        }
        let v = t.derive(&self.history, key, &[]).await?;
        self.history
            .put_derived(vec![DerivedRow {
                key: *storage,
                type_name: t.name().into(),
                version: t.version(),
                value: v.clone(),
            }])
            .await?;
        Ok(v)
    }

    /// A stored value of any version, without deriving (old versions stay readable until
    /// GC removes them).
    pub async fn read_stored(
        &self,
        type_name: &str,
        version: u32,
        key: &ObjectId,
    ) -> Result<Option<Vec<u8>>> {
        let storage = match self.registry.get(type_name) {
            Some(t) => t.storage_key(key),
            None => *key,
        };
        self.history.derived(&storage, type_name, version).await
    }

    /// The `graph` row of a changeset.
    pub async fn graph(&self, changeset: &ObjectId) -> Result<GraphNode> {
        self.history.graph_node(changeset).await
    }

    /// The `diff_class` of a changeset.
    pub async fn diff_class(&self, changeset: &ObjectId) -> Result<DiffClassValue> {
        self.history.changeset(changeset).await?;
        DiffClassValue::decode(&self.get(DIFF_CLASS_TYPE, changeset).await?.value)
    }

    /// The copies and renames a changeset made.
    pub async fn copies(&self, changeset: &ObjectId) -> Result<Vec<CopyRecord>> {
        self.history.changeset(changeset).await?;
        decode_copies(&self.get(COPY_DETECTION_TYPE, changeset).await?.value)
    }

    /// Language statistics of a changeset's tree.
    pub async fn language_stats(&self, changeset: &ObjectId) -> Result<LanguageStats> {
        self.history.changeset(changeset).await?;
        decode_stats(&self.get(LANGUAGE_STATS_TYPE, changeset).await?.value)
    }

    /// The git commit an imported changeset came from (raw id bytes).
    pub async fn git_oid(&self, changeset: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.history.git_oid_of(changeset).await
    }
}
