//! The derived-type contract (SPEC §8).

use async_trait::async_trait;
use bee_core::{ObjectId, Result};
use bee_history::History;
use bee_object::Changeset;
use bee_store::DerivedRow;

/// When a type's values are computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// In CreateChangeset's transaction, for every changeset.
    Eager,
    /// On first read, deriving forward from the nearest stored values.
    Lazy,
    /// Written by an outside producer (the importer); never derived here.
    Importer,
}

impl Mode {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Eager => "eager",
            Mode::Lazy => "lazy",
            Mode::Importer => "importer",
        }
    }
}

/// What a type's request key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyKind {
    /// A changeset id.
    Changeset,
    /// A file history node (`d110_…`, see `bee_history::pathhist`).
    HistoryNode,
}

/// A derived type: `{name, version, mode, key, derive}`. `derive` MUST be deterministic:
/// the same key and dependency values always give the same bytes.
#[async_trait]
pub trait DerivedType: Send + Sync {
    /// The type name (the `derived` table's `type` column).
    fn name(&self) -> &'static str;

    /// The type version; bumping it re-derives on demand with no client change.
    fn version(&self) -> u32;

    /// Eager, lazy or importer-written.
    fn mode(&self) -> Mode;

    /// What request keys name.
    fn key_kind(&self) -> KeyKind;

    /// True when values depend on blob contents (so redaction and path permissions matter
    /// to readers).
    fn content_bearing(&self) -> bool;

    /// The storage key for a request key. Types parameterised by configuration fold it in
    /// here; the default is the request key itself.
    fn storage_key(&self, key: &ObjectId) -> ObjectId {
        *key
    }

    /// True when [`DerivedType::derive`] needs other keys' values ([`DerivedType::deps`]);
    /// such types must use the request key as the storage key.
    fn has_deps(&self) -> bool {
        false
    }

    /// The keys whose values `derive` needs (for example a changeset's first parent).
    async fn deps(&self, history: &History, key: &ObjectId) -> Result<Vec<ObjectId>> {
        let _ = (history, key);
        Ok(Vec::new())
    }

    /// Derives the value for a request key from its dependencies' values.
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>>;

    /// The rows an eager type contributes to a new changeset's transaction.
    async fn eager_rows(
        &self,
        history: &History,
        id: ObjectId,
        changeset: &Changeset,
    ) -> Result<Vec<DerivedRow>> {
        let _ = (history, id, changeset);
        Ok(Vec::new())
    }
}

/// A value read through the generic API (GetDerived).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedValue {
    /// Type name.
    pub type_name: String,
    /// Type version.
    pub version: u32,
    /// The request key.
    pub key: ObjectId,
    /// The encoded value (each type documents its encoding).
    pub value: Vec<u8>,
    /// True when this read derived the value (it was not stored before).
    pub derived: bool,
}
