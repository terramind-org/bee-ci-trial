//! `copy_detection` (lazy, PRODUCT §4.4): where each file a changeset adds came from.
//!
//! For every path the delta adds (absent as a file in the first parent):
//!
//! 1. **Exact**: a file of the first parent with the same blob, found through the
//!    `object_paths` index (no tree scan), preferring the same basename, then path order.
//! 2. **Similar**: otherwise the most similar text file among those the delta deletes or
//!    modifies (their first-parent versions), by line-set similarity ≥ 1/2 (`diff3-v1` §6.1),
//!    ties broken by same basename then path order.
//!
//! A source the delta deletes is a **rename** (each deleted file is the rename source of at
//! most one added file, in path order); any other source is a **copy**. Value, keyed by the
//! changeset:
//!
//! ```text
//! {1: [{1: path, 2: source path, 3: kind (0 copy, 1 rename), 4: shared, 5: union}, ...]}
//! ```
//!
//! (`4` and `5`, distinct lines shared and in either file, are omitted for exact matches.)

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use bee_core::{ObjectId, RepoPath, Result};
use bee_history::codec::{self, Fields, corrupt};
use bee_history::{BlobContent, History};
use bee_merge::DEFAULT_MAX_TEXT_BYTES;
use bee_object::{DeltaOp, Value};
use bee_store::ReadFrom;

use crate::types::{DerivedType, KeyKind, Mode};

/// The derived type name.
pub const COPY_DETECTION_TYPE: &str = "copy_detection";
/// The derived type version.
pub const COPY_DETECTION_VERSION: u32 = 1;
/// Exact-match candidates examined per added file.
const MAX_EXACT_CANDIDATES: usize = 64;

/// Copy or rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CopyKind {
    /// The source still exists.
    Copy,
    /// The source was deleted by the same changeset.
    Rename,
}

/// One detected copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRecord {
    /// The added path.
    pub path: RepoPath,
    /// Where it came from (a path of the first parent).
    pub source: RepoPath,
    /// Copy or rename.
    pub kind: CopyKind,
    /// `None` for identical content, else `(shared, union)` distinct lines.
    pub similarity: Option<(u64, u64)>,
}

/// Encodes records.
pub fn encode_copies(records: &[CopyRecord]) -> Vec<u8> {
    codec::map([(
        1,
        Value::Array(
            records
                .iter()
                .map(|r| {
                    let mut m = BTreeMap::new();
                    m.insert(1, Value::Bytes(r.path.as_bytes().to_vec()));
                    m.insert(2, Value::Bytes(r.source.as_bytes().to_vec()));
                    m.insert(
                        3,
                        Value::Uint(match r.kind {
                            CopyKind::Copy => 0,
                            CopyKind::Rename => 1,
                        }),
                    );
                    if let Some((s, u)) = r.similarity {
                        m.insert(4, Value::Uint(s));
                        m.insert(5, Value::Uint(u));
                    }
                    Value::Map(m)
                })
                .collect(),
        ),
    )])
    .encode()
}

/// Decodes records.
pub fn decode_copies(bytes: &[u8]) -> Result<Vec<CopyRecord>> {
    let what = COPY_DETECTION_TYPE;
    let f = Fields::decode(what, bytes)?;
    let mut out = Vec::new();
    for v in f.opt_array(1)? {
        let r = Fields::from_value(what, v)?;
        let path =
            |k| -> Result<RepoPath> { RepoPath::new(r.bytes(k)?).map_err(|e| corrupt(what, e)) };
        out.push(CopyRecord {
            path: path(1)?,
            source: path(2)?,
            kind: match r.uint(3)? {
                0 => CopyKind::Copy,
                1 => CopyKind::Rename,
                _ => return Err(corrupt(what, "bad kind")),
            },
            similarity: match (r.opt_uint(4)?, r.opt_uint(5)?) {
                (Some(s), Some(u)) => Some((s, u)),
                _ => None,
            },
        });
    }
    Ok(out)
}

/// The `copy_detection` type.
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyDetectionType;

fn text(content: &BlobContent) -> Option<&[u8]> {
    match content {
        BlobContent::Bytes(b)
            if !b.is_empty() && !bee_merge::is_binary(b, DEFAULT_MAX_TEXT_BYTES) =>
        {
            Some(b)
        }
        _ => None,
    }
}

#[async_trait]
impl DerivedType for CopyDetectionType {
    fn name(&self) -> &'static str {
        COPY_DETECTION_TYPE
    }
    fn version(&self) -> u32 {
        COPY_DETECTION_VERSION
    }
    fn mode(&self) -> Mode {
        Mode::Lazy
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::Changeset
    }
    fn content_bearing(&self) -> bool {
        true
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        _deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        let cs = history.changeset(key).await?;
        let Some(parent) = cs.first_parent() else {
            return Ok(encode_copies(&[]));
        };
        let root = *history.changeset(parent).await?.root();
        let mut added: Vec<(RepoPath, ObjectId, bee_core::Mode)> = Vec::new();
        let mut deleted: BTreeSet<RepoPath> = BTreeSet::new();
        let mut sources: BTreeMap<RepoPath, (bee_core::Mode, ObjectId)> = BTreeMap::new();
        for e in cs.delta() {
            let old = history
                .entry_at(&root, &e.path)
                .await?
                .filter(|o| o.is_leaf());
            if let Some(o) = &old {
                sources.insert(e.path.clone(), (o.mode, o.id));
            }
            match e.op {
                DeltaOp::Delete => {
                    deleted.insert(e.path.clone());
                }
                DeltaOp::Put { mode, id, .. } if old.is_none() => {
                    added.push((e.path.clone(), id, mode))
                }
                DeltaOp::Put { .. } => {}
            }
        }
        let mut records = Vec::new();
        let mut renamed: BTreeSet<RepoPath> = BTreeSet::new();
        let mut kind_for = |src: &RepoPath| {
            if deleted.contains(src) && renamed.insert(src.clone()) {
                CopyKind::Rename
            } else {
                CopyKind::Copy
            }
        };
        let mut unmatched = Vec::new();
        for (path, id, mode) in &added {
            let mut candidates: Vec<RepoPath> = history
                .meta()
                .object_paths(history.repo(), id, ReadFrom::Primary)
                .await?
                .into_iter()
                .flatten()
                .filter(|p| p != path)
                .collect();
            candidates.sort();
            candidates.truncate(MAX_EXACT_CANDIDATES);
            let mut ok = Vec::new();
            for c in candidates {
                if let Some(e) = history.entry_at(&root, &c).await?
                    && e.is_leaf()
                    && e.id == *id
                {
                    ok.push(c);
                }
            }
            let pick = ok
                .iter()
                .find(|c| c.file_name() == path.file_name())
                .or(ok.first())
                .cloned();
            match pick {
                Some(source) => records.push(CopyRecord {
                    path: path.clone(),
                    kind: kind_for(&source),
                    source,
                    similarity: None,
                }),
                None => unmatched.push((path.clone(), *id, *mode)),
            }
        }
        if !unmatched.is_empty() && !sources.is_empty() {
            let mut loaded: Vec<(RepoPath, BlobContent)> = Vec::new();
            for (p, (mode, id)) in &sources {
                loaded.push((p.clone(), history.leaf_content(*mode, id).await?));
            }
            for (path, id, mode) in unmatched {
                let content = history.leaf_content(mode, &id).await?;
                let Some(new) = text(&content) else { continue };
                let mut best: Option<(u64, u64, bool, RepoPath)> = None;
                for (src, sc) in &loaded {
                    let Some(old) = text(sc) else { continue };
                    let (shared, union) = bee_merge::line_similarity(old, new);
                    if union == 0 || shared * 2 < union {
                        continue;
                    }
                    let same = src.file_name() == path.file_name();
                    let better = match &best {
                        None => true,
                        Some((bs, bu, bsame, _)) => {
                            let lhs = u128::from(shared) * u128::from(*bu);
                            let rhs = u128::from(*bs) * u128::from(union);
                            lhs > rhs || (lhs == rhs && same && !bsame)
                        }
                    };
                    if better {
                        best = Some((shared, union, same, src.clone()));
                    }
                }
                if let Some((shared, union, _, source)) = best {
                    records.push(CopyRecord {
                        path,
                        kind: kind_for(&source),
                        source,
                        similarity: Some((shared, union)),
                    });
                }
            }
        }
        records.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(encode_copies(&records))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips() {
        let p = |s: &str| RepoPath::new(s.as_bytes()).unwrap();
        let rs = vec![
            CopyRecord {
                path: p("b"),
                source: p("a"),
                kind: CopyKind::Rename,
                similarity: None,
            },
            CopyRecord {
                path: p("c"),
                source: p("a"),
                kind: CopyKind::Copy,
                similarity: Some((3, 4)),
            },
        ];
        assert_eq!(decode_copies(&encode_copies(&rs)).unwrap(), rs);
    }
}
