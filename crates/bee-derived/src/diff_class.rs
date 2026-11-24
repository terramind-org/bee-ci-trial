//! `diff_class` (lazy, SPEC §8, §11.3): the semantic classification of a changeset's delta
//! against its first parent, computed with `bee-classify`.
//!
//! Renames are paired first (exact content, then line similarity ≥ 1/2, `diff3-v1` §6.1) so a
//! move is classified as one change. Value, keyed by the changeset (or, with policy rules, by
//! a derived key folding in the rules' fingerprint, kind `0x12`):
//!
//! ```text
//! {1: highest class (text, omitted for an empty delta), 2: [[path, class, basis], ...],
//!  3: classifier version (text)}
//! ```

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use bee_classify::{Basis, CLASSIFIER_VERSION, Class, ClassRules, Content, PathChange};
use bee_core::{Mode as EntryMode, ObjectId, ObjectKind, RepoPath, Result};
use bee_history::codec::{Fields, as_array, as_bytes, corrupt};
use bee_history::merge::{merge_mode, side_of};
use bee_history::{BlobContent, History};
use bee_merge::{MergeOptions, RenameOptions};
use bee_object::{DeltaOp, Value, digest};

use crate::types::{DerivedType, KeyKind, Mode};

/// The derived type name.
pub const DIFF_CLASS_TYPE: &str = "diff_class";
/// The derived type version.
pub const DIFF_CLASS_VERSION: u32 = 1;
/// The derived kind byte of keys under non-default rules.
pub const DIFF_CLASS_KEY_KIND: u8 = 0x12;

/// The decoded value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffClassValue {
    /// The highest-risk class present; `None` for an empty delta.
    pub highest: Option<Class>,
    /// Per path (the new path for a rename): class and the rule that decided it.
    pub per_path: Vec<(RepoPath, Class, String)>,
    /// The classifier version.
    pub classifier: String,
}

/// The stable name of a [`Basis`].
pub fn basis_name(b: Basis) -> &'static str {
    match b {
        Basis::SensitivePath => "sensitive_path",
        Basis::GeneratedPath => "generated_path",
        Basis::DocsPath => "docs_path",
        Basis::TestPath => "test_path",
        Basis::DependencyPath => "dependency_path",
        Basis::Unchanged => "unchanged",
        Basis::CommentsAndWhitespace => "comments_and_whitespace",
        Basis::IdenticalMove => "identical_move",
        Basis::Fallback => "fallback",
    }
}

impl DiffClassValue {
    /// Encodes the value.
    pub fn encode(&self) -> Vec<u8> {
        let mut m = BTreeMap::new();
        if let Some(h) = self.highest {
            m.insert(1, Value::Text(h.as_str().into()));
        }
        m.insert(
            2,
            Value::Array(
                self.per_path
                    .iter()
                    .map(|(p, c, b)| {
                        Value::Array(vec![
                            Value::Bytes(p.as_bytes().to_vec()),
                            Value::Text(c.as_str().into()),
                            Value::Text(b.clone()),
                        ])
                    })
                    .collect(),
            ),
        );
        m.insert(3, Value::Text(self.classifier.clone()));
        Value::Map(m).encode()
    }

    /// Decodes a stored value.
    pub fn decode(bytes: &[u8]) -> Result<DiffClassValue> {
        let f = Fields::decode(DIFF_CLASS_TYPE, bytes)?;
        let class = |t: &str| Class::parse(t).ok_or_else(|| corrupt(DIFF_CLASS_TYPE, "bad class"));
        let highest = if f.has(1) {
            Some(class(&f.text(1)?)?)
        } else {
            None
        };
        let mut per_path = Vec::new();
        for row in f.array(2)? {
            let r = as_array(DIFF_CLASS_TYPE, &row)?;
            let text = |v: &Value| match v {
                Value::Text(t) => Ok(t.clone()),
                _ => Err(corrupt(DIFF_CLASS_TYPE, "expected text")),
            };
            if r.len() != 3 {
                return Err(corrupt(DIFF_CLASS_TYPE, "bad row"));
            }
            per_path.push((
                RepoPath::new(as_bytes(DIFF_CLASS_TYPE, &r[0])?)
                    .map_err(|e| corrupt(DIFF_CLASS_TYPE, e))?,
                class(&text(&r[1])?)?,
                text(&r[2])?,
            ));
        }
        Ok(DiffClassValue {
            highest,
            per_path,
            classifier: f.text(3)?,
        })
    }
}

/// `diff_class` under a set of rules.
#[derive(Clone, Debug, Default)]
pub struct DiffClassType {
    rules: ClassRules,
    fingerprint: String,
}

impl DiffClassType {
    /// Classification under policy-supplied rules. `fingerprint` must change whenever the
    /// rules do (for example the policy epoch); it is folded into the storage key so values
    /// under different rules never mix. An empty fingerprint means the default rules.
    pub fn with_rules(rules: ClassRules, fingerprint: impl Into<String>) -> DiffClassType {
        DiffClassType {
            rules,
            fingerprint: fingerprint.into(),
        }
    }
}

/// One side of a changed path, loaded.
struct Loaded {
    mode: EntryMode,
    id: ObjectId,
    content: BlobContent,
}

impl Loaded {
    fn classify_content(&self) -> Content {
        Content {
            bytes: self.content.bytes_or_empty().to_vec(),
            mode: self.mode,
        }
    }
}

/// `(path, old, new, renamed_from)`.
type LoadedChange = (RepoPath, Option<Loaded>, Option<Loaded>, Option<RepoPath>);

/// The delta of `cs` against its first parent with both sides loaded, renames paired.
struct LoadedDelta {
    changes: Vec<LoadedChange>,
}

async fn load_delta(history: &History, cs_id: &ObjectId) -> Result<LoadedDelta> {
    let cs = history.changeset(cs_id).await?;
    let parent_root = match cs.first_parent() {
        Some(p) => Some(*history.changeset(p).await?.root()),
        None => None,
    };
    let mut olds: BTreeMap<RepoPath, Loaded> = BTreeMap::new();
    let mut news: BTreeMap<RepoPath, Loaded> = BTreeMap::new();
    for e in cs.delta() {
        if let Some(root) = &parent_root
            && let Some(old) = history.entry_at(root, &e.path).await?
            && old.is_leaf()
        {
            let content = history.leaf_content(old.mode, &old.id).await?;
            olds.insert(
                e.path.clone(),
                Loaded {
                    mode: old.mode,
                    id: old.id,
                    content,
                },
            );
        }
        if let DeltaOp::Put { mode, id, .. } = e.op {
            let content = history.leaf_content(mode, &id).await?;
            news.insert(e.path.clone(), Loaded { mode, id, content });
        }
    }
    let base: bee_merge::Tree<'_> = olds
        .iter()
        .filter(|(p, _)| !news.contains_key(*p))
        .map(|(p, l)| {
            (
                p.as_bytes().to_vec(),
                side_of(merge_mode(l.mode), &l.content, &l.id),
            )
        })
        .collect();
    let side: bee_merge::Tree<'_> = news
        .iter()
        .filter(|(p, _)| !olds.contains_key(*p))
        .map(|(p, l)| {
            (
                p.as_bytes().to_vec(),
                side_of(merge_mode(l.mode), &l.content, &l.id),
            )
        })
        .collect();
    let renames: BTreeMap<Vec<u8>, Vec<u8>> = bee_merge::detect_renames(
        &base,
        &side,
        &RenameOptions::default(),
        &MergeOptions::default(),
    )
    .into_iter()
    .map(|r| (r.to, r.from))
    .collect();
    let gone: BTreeSet<Vec<u8>> = renames.values().cloned().collect();
    let mut changes = Vec::new();
    let paths: BTreeSet<RepoPath> = olds.keys().chain(news.keys()).cloned().collect();
    for p in paths {
        if gone.contains(p.as_bytes()) {
            continue;
        }
        let new = news.remove(&p);
        match renames.get(p.as_bytes()) {
            Some(from) => {
                let from = RepoPath::new(from.clone())?;
                let old = olds.remove(&from);
                changes.push((p, old, new, Some(from)));
            }
            None => {
                let old = olds.remove(&p);
                changes.push((p, old, new, None));
            }
        }
    }
    Ok(LoadedDelta { changes })
}

#[async_trait]
impl DerivedType for DiffClassType {
    fn name(&self) -> &'static str {
        DIFF_CLASS_TYPE
    }
    fn version(&self) -> u32 {
        DIFF_CLASS_VERSION
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
    fn storage_key(&self, key: &ObjectId) -> ObjectId {
        if self.fingerprint.is_empty() {
            return *key;
        }
        let mut input = b"swarm-diff-class\0".to_vec();
        input.extend_from_slice(key.as_bytes());
        input.extend_from_slice(self.fingerprint.as_bytes());
        ObjectId::new(ObjectKind::Derived(DIFF_CLASS_KEY_KIND), digest(&input))
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        _deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        let loaded = load_delta(history, key).await?;
        let inputs: Vec<PathChange> = loaded
            .changes
            .iter()
            .map(|(p, old, new, from)| PathChange {
                path: p.clone(),
                old: old.as_ref().map(Loaded::classify_content),
                new: new.as_ref().map(Loaded::classify_content),
                renamed_from: from.clone(),
            })
            .collect();
        let out = bee_classify::classify_change(&inputs, &self.rules);
        Ok(DiffClassValue {
            highest: out.highest,
            per_path: out
                .per_path
                .into_iter()
                .map(|c| (c.path, c.class, basis_name(c.basis).to_string()))
                .collect(),
            classifier: CLASSIFIER_VERSION.into(),
        }
        .encode())
    }
}
