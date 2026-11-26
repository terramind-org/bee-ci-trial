//! The types whose rows and algorithms live in `bee-history` (which needs them below this
//! crate's layer): `graph`, `path_history`, `blame` and `git_mapping`. These adapters expose
//! them through the framework without a second implementation.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason, Result};
use bee_history::blame::{BLAME_TYPE, BLAME_VERSION, BlameRows};
use bee_history::gitmap::{GIT_MAPPING_TYPE, GIT_MAPPING_VERSION};
use bee_history::graph::GraphRows;
use bee_history::pathhist::PathHistoryRows;
use bee_history::{
    Forward, GRAPH_TYPE, GRAPH_VERSION, History, PATH_HISTORY_TYPE, PATH_HISTORY_VERSION,
};

use crate::types::{DerivedType, KeyKind, Mode};

/// `graph` (eager): generation, skip pointers, first-parent merge counts, parents.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphType;

#[async_trait]
impl DerivedType for GraphType {
    fn name(&self) -> &'static str {
        GRAPH_TYPE
    }
    fn version(&self) -> u32 {
        GRAPH_VERSION
    }
    fn mode(&self) -> Mode {
        Mode::Eager
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::Changeset
    }
    fn content_bearing(&self) -> bool {
        false
    }
    fn has_deps(&self) -> bool {
        true
    }
    async fn deps(&self, history: &History, key: &ObjectId) -> Result<Vec<ObjectId>> {
        GraphRows::new(history).deps(key).await
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        GraphRows::new(history).derive(key, deps).await
    }
}

/// `path_history` (eager): per-path history nodes; keyed by changeset, the value names the
/// root node (node rows are written alongside).
#[derive(Clone, Copy, Debug, Default)]
pub struct PathHistoryType;

#[async_trait]
impl DerivedType for PathHistoryType {
    fn name(&self) -> &'static str {
        PATH_HISTORY_TYPE
    }
    fn version(&self) -> u32 {
        PATH_HISTORY_VERSION
    }
    fn mode(&self) -> Mode {
        Mode::Eager
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::Changeset
    }
    fn content_bearing(&self) -> bool {
        false
    }
    fn has_deps(&self) -> bool {
        true
    }
    async fn deps(&self, history: &History, key: &ObjectId) -> Result<Vec<ObjectId>> {
        PathHistoryRows::new(history).deps(key).await
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        PathHistoryRows::new(history).derive(key, deps).await
    }
}

/// `blame` (lazy): line attribution of a file history node (no ignore list; blame with an
/// ignore list is cached under its own keys by `History::blame`).
#[derive(Clone, Copy, Debug, Default)]
pub struct BlameType;

#[async_trait]
impl DerivedType for BlameType {
    fn name(&self) -> &'static str {
        BLAME_TYPE
    }
    fn version(&self) -> u32 {
        BLAME_VERSION
    }
    fn mode(&self) -> Mode {
        Mode::Lazy
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::HistoryNode
    }
    fn content_bearing(&self) -> bool {
        true
    }
    fn has_deps(&self) -> bool {
        true
    }
    async fn deps(&self, history: &History, key: &ObjectId) -> Result<Vec<ObjectId>> {
        let rows = BlameRows::new(history, BTreeSet::new());
        rows.key_for(key);
        rows.deps(key).await
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        let rows = BlameRows::new(history, BTreeSet::new());
        rows.key_for(key);
        for (d, _) in deps {
            rows.key_for(d);
        }
        rows.derive(key, deps).await
    }
}

/// `git_mapping` (importer): changeset → raw git commit id, and the reverse under the
/// commit's foreign id. Never derived here.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitMappingType;

#[async_trait]
impl DerivedType for GitMappingType {
    fn name(&self) -> &'static str {
        GIT_MAPPING_TYPE
    }
    fn version(&self) -> u32 {
        GIT_MAPPING_VERSION
    }
    fn mode(&self) -> Mode {
        Mode::Importer
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::Changeset
    }
    fn content_bearing(&self) -> bool {
        false
    }
    async fn derive(
        &self,
        _history: &History,
        key: &ObjectId,
        _deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        Err(BeeError::new(
            Reason::NotFound,
            format!("{key} was not imported from git"),
        ))
    }
}
