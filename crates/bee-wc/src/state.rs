//! `.bee/` state (SPEC §10.3): the config (endpoint, repo, target ref) as JSON, and the state
//! (checked-out changeset, local stack, sparse profile, rebase in progress) as canonical TOML
//! (`state.toml`: keys in declaration order, absent optional values omitted, one encoding per
//! value), each written atomically. A `state.json` from an older client is read and replaced.

use bee_core::{ChangeId, ObjectId};
use serde::{Deserialize, Serialize};

use crate::error::{Result, WcError};
use crate::manifest::{Entry, Manifest};
use crate::sparse::Sparse;

/// Serde helpers for ids in text form.
pub mod ids {
    use bee_core::{ChangeId, ObjectId};
    use serde::{Deserialize, Deserializer, Serializer};

    /// An `ObjectId` as `c1_…` text.
    pub mod object {
        use super::*;

        /// Serializes in text form.
        pub fn serialize<S: Serializer>(id: &ObjectId, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&id.to_text())
        }

        /// Parses the text form.
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ObjectId, D::Error> {
            let t = String::deserialize(d)?;
            ObjectId::parse(&t).map_err(serde::de::Error::custom)
        }
    }

    /// An optional `ObjectId` as text or null.
    pub mod object_opt {
        use super::*;

        /// Serializes in text form.
        pub fn serialize<S: Serializer>(id: &Option<ObjectId>, s: S) -> Result<S::Ok, S::Error> {
            match id {
                Some(id) => s.serialize_some(&id.to_text()),
                None => s.serialize_none(),
            }
        }

        /// Parses the text form.
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ObjectId>, D::Error> {
            match Option::<String>::deserialize(d)? {
                Some(t) => ObjectId::parse(&t)
                    .map(Some)
                    .map_err(serde::de::Error::custom),
                None => Ok(None),
            }
        }
    }

    /// A `ChangeId` as `k_…` text.
    pub mod change {
        use super::*;

        /// Serializes in text form.
        pub fn serialize<S: Serializer>(id: &ChangeId, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&id.to_text())
        }

        /// Parses the text form.
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ChangeId, D::Error> {
            let t = String::deserialize(d)?;
            ChangeId::parse(&t).map_err(serde::de::Error::custom)
        }
    }

    /// An optional `ChangeId`.
    pub mod change_opt {
        use super::*;

        /// Serializes in text form.
        pub fn serialize<S: Serializer>(id: &Option<ChangeId>, s: S) -> Result<S::Ok, S::Error> {
            match id {
                Some(id) => s.serialize_some(&id.to_text()),
                None => s.serialize_none(),
            }
        }

        /// Parses the text form.
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ChangeId>, D::Error> {
            match Option::<String>::deserialize(d)? {
                Some(t) => ChangeId::parse(&t)
                    .map(Some)
                    .map_err(serde::de::Error::custom),
                None => Ok(None),
            }
        }
    }
}

/// A repo path in JSON: the text when it is UTF-8, else `%hex:` and its hex bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathBytes(pub Vec<u8>);

const HEX_TAG: &str = "%hex:";

impl Serialize for PathBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match std::str::from_utf8(&self.0) {
            Ok(t) if !t.starts_with(HEX_TAG) => s.serialize_str(t),
            _ => s.serialize_str(&format!("{HEX_TAG}{}", bee_core::hex::encode(&self.0))),
        }
    }
}

impl<'de> Deserialize<'de> for PathBytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let t = String::deserialize(d)?;
        match t.strip_prefix(HEX_TAG) {
            Some(h) => bee_core::hex::decode(h)
                .map(PathBytes)
                .ok_or_else(|| serde::de::Error::custom("bad hex path")),
            None => Ok(PathBytes(t.into_bytes())),
        }
    }
}

/// `.bee/config.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Format version (1).
    pub version: u32,
    /// Server endpoint the working copy was made from, if known.
    pub endpoint: Option<String>,
    /// Repo, `org/name` or an id.
    pub repo: String,
    /// The protected ref this working copy syncs with and lands on.
    pub target_ref: String,
    /// 32 random bytes (hex) chosen at init; ChangeIds derive from it and a counter.
    pub seed: String,
}

/// One change of the local stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackEntry {
    /// Stable ChangeId.
    #[serde(with = "ids::change")]
    pub change_id: ChangeId,
    /// Current changeset of the change.
    #[serde(with = "ids::object")]
    pub changeset: ObjectId,
    /// Its first parent.
    #[serde(with = "ids::object")]
    pub parent: ObjectId,
    /// Its message.
    pub message: String,
}

/// One conflicting path of a rebase in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRecord {
    /// The path.
    pub path: PathBytes,
    /// Conflict kind: `content`, `add_add`, `modify_delete`, `symlink`, `binary`,
    /// `redacted`, `mode`, `file_directory`.
    pub kind: String,
    /// Base side.
    pub base: Option<Entry>,
    /// Ours: the side being rebased onto (the target).
    pub ours: Option<Entry>,
    /// Theirs: the change being rebased.
    pub theirs: Option<Entry>,
    /// What was written to the working tree (`None` = nothing at the path).
    pub rendered: Option<Entry>,
    /// The file holds diff3 markers.
    pub markers: bool,
    /// Marked resolved by `bee resolve`.
    pub resolved: bool,
}

/// Why a rebase is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebaseKind {
    /// `bee sync`: the stack moves onto the target ref.
    Sync,
    /// A rewritten change's descendants move onto its new version.
    Restack,
}

/// A rebase in progress: done, current (conflicted) and pending changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebaseOp {
    /// Why it runs.
    pub kind: RebaseKind,
    /// Parent for the next change to move.
    #[serde(with = "ids::object")]
    pub base: ObjectId,
    /// Stack entries below the moved range, unchanged.
    pub kept: Vec<StackEntry>,
    /// Entries already moved.
    pub done: Vec<StackEntry>,
    /// The entry that conflicted (its old version), if stopped.
    pub current: Option<StackEntry>,
    /// Entries still to move.
    pub pending: Vec<StackEntry>,
    /// Conflicts of `current`.
    pub conflicts: Vec<ConflictRecord>,
    /// Clean paths of `current` as merged by the server (`None` = absent).
    #[serde(with = "clean_paths")]
    pub clean: Vec<(PathBytes, Option<Entry>)>,
    /// ChangeIds dropped because they became empty.
    pub dropped: Vec<String>,
    /// Where the stack bottom sits afterwards.
    #[serde(with = "ids::object")]
    pub new_onto: ObjectId,
    /// Target ref version of `new_onto`.
    pub new_onto_version: u64,
    /// Change to check out afterwards; the top of the stack when `None`.
    #[serde(with = "ids::change_opt")]
    pub return_to: Option<ChangeId>,
    /// Check out the stack bottom (`new_onto`) afterwards instead.
    #[serde(default)]
    pub return_bottom: bool,
    /// State to restore on abort.
    pub abort: Box<State>,
}

/// `RebaseOp.clean` as a list of `{path, entry?}` tables (TOML has no null), reading the older
/// `[path, entry | null]` pairs too.
mod clean_paths {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::PathBytes;
    use crate::manifest::Entry;

    #[derive(Serialize, Deserialize)]
    struct Item {
        path: PathBytes,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<Entry>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Table(Item),
        Pair(PathBytes, Option<Entry>),
    }

    pub fn serialize<S: Serializer>(
        v: &[(PathBytes, Option<Entry>)],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let items: Vec<Item> = v
            .iter()
            .map(|(path, entry)| Item {
                path: path.clone(),
                entry: *entry,
            })
            .collect();
        items.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<(PathBytes, Option<Entry>)>, D::Error> {
        Ok(Vec::<Either>::deserialize(d)?
            .into_iter()
            .map(|e| match e {
                Either::Table(i) => (i.path, i.entry),
                Either::Pair(p, e) => (p, e),
            })
            .collect())
    }
}

/// A tracked path deliberately absent from disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenPath {
    /// The path.
    pub path: PathBytes,
    /// Why: `case_collision`, `symlinked_parent`, `redacted`, `not_readable`, `gitlink`.
    pub reason: String,
}

/// `.bee/state.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// Format version (1).
    pub version: u32,
    /// The checked-out changeset: the working tree's base.
    #[serde(with = "ids::object")]
    pub head: ObjectId,
    /// Target ref tip the stack is based on.
    #[serde(with = "ids::object")]
    pub onto: ObjectId,
    /// Target ref version of `onto`.
    pub onto_version: u64,
    /// Local changes not landed yet, bottom first.
    pub stack: Vec<StackEntry>,
    /// Sparse profile.
    pub sparse: Sparse,
    /// Rebase in progress.
    pub rebase: Option<RebaseOp>,
    /// Tracked paths not written to disk.
    pub hidden: Vec<HiddenPath>,
    /// Counter for ChangeId derivation.
    pub next_change: u64,
    /// The repo's redaction epoch this working copy last applied (SPEC §13): mutating
    /// commands ask for redactions made since and refresh placeholders.
    #[serde(default)]
    pub redaction_epoch: u64,
}

impl State {
    /// The stack index of `head`, if it is a stack entry.
    pub fn head_index(&self) -> Option<usize> {
        self.stack.iter().position(|e| e.changeset == self.head)
    }

    /// The stack index of the change with this ChangeId.
    pub fn index_of(&self, change: &ChangeId) -> Option<usize> {
        self.stack.iter().position(|e| e.change_id == *change)
    }

    /// The hidden paths as a set.
    pub fn hidden_set(&self) -> std::collections::BTreeSet<Vec<u8>> {
        self.hidden.iter().map(|h| h.path.0.clone()).collect()
    }
}

/// What the working tree should hold for `state` on top of its base `manifest`: the base,
/// plus a rebase's clean paths and rendered conflicts.
pub fn expected_tree(state: &State, manifest: &Manifest) -> Manifest {
    let mut out = manifest.clone();
    if let Some(op) = &state.rebase
        && op.current.is_some()
    {
        for (p, e) in &op.clean {
            if !state.sparse.contains(&p.0) {
                continue;
            }
            match e {
                Some(e) => {
                    out.insert(p.0.clone(), *e);
                }
                None => {
                    out.remove(&p.0);
                }
            }
        }
        for c in &op.conflicts {
            if !state.sparse.contains(&c.path.0) {
                continue;
            }
            match &c.rendered {
                Some(e) => {
                    out.insert(c.path.0.clone(), *e);
                }
                None => {
                    out.remove(&c.path.0);
                }
            }
        }
    }
    out
}

/// Parses JSON from a state file.
pub fn parse<T: serde::de::DeserializeOwned>(what: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| WcError::integrity("corrupt_file", format!("{what}: {e}")))
}

/// The canonical TOML of a state (`state.toml`).
pub fn to_toml(state: &State) -> Result<Vec<u8>> {
    toml::to_string(state)
        .map(String::into_bytes)
        .map_err(|e| WcError::internal("encode", e.to_string()))
}

/// Parses `state.toml`.
pub fn parse_toml(bytes: &[u8]) -> Result<State> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| WcError::integrity("corrupt_file", format!("state.toml: {e}")))?;
    let state: State = toml::from_str(text)
        .map_err(|e| WcError::integrity("corrupt_file", format!("state.toml: {e}")))?;
    if state.version != 1 {
        return Err(WcError::integrity(
            "unsupported_version",
            format!("state.toml: version {} is not supported", state.version),
        ));
    }
    Ok(state)
}

/// Reads the state of the working copy whose `.bee/` is `dir`: `state.toml`, else a
/// `state.json` an older client wrote; `None` when neither exists.
pub fn read_state(dir: &std::path::Path) -> Result<Option<State>> {
    if let Some(bytes) = crate::fsutil::read_optional(&dir.join("state.toml"))? {
        return parse_toml(&bytes).map(Some);
    }
    match crate::fsutil::read_optional(&dir.join("state.json"))? {
        Some(bytes) => parse("state.json", &bytes).map(Some),
        None => Ok(None),
    }
}

/// Serializes state as pretty JSON with a trailing newline.
pub fn to_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut out =
        serde_json::to_vec_pretty(value).map_err(|e| WcError::internal("encode", e.to_string()))?;
    out.push(b'\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;

    #[test]
    fn paths_roundtrip_including_non_utf8() {
        for p in [b"src/a.rs".to_vec(), vec![0xff, b'x'], b"%hex:abc".to_vec()] {
            let text = serde_json::to_string(&PathBytes(p.clone())).expect("ser");
            let back: PathBytes = serde_json::from_str(&text).expect("de");
            assert_eq!(back.0, p, "{text}");
        }
    }

    #[test]
    fn state_roundtrip() {
        let id = ObjectId::new(ObjectKind::Changeset, [1; 32]);
        let state = State {
            version: 1,
            head: id,
            onto: id,
            onto_version: 3,
            stack: vec![StackEntry {
                change_id: ChangeId::new([2; 16]),
                changeset: id,
                parent: id,
                message: "m".into(),
            }],
            sparse: Sparse::from_prefixes(["a"]).expect("sparse"),
            rebase: None,
            hidden: vec![HiddenPath {
                path: PathBytes(b"A".to_vec()),
                reason: "case_collision".into(),
            }],
            next_change: 5,
            redaction_epoch: 2,
        };
        let bytes = to_json(&state).expect("json");
        let back: State = parse("state", &bytes).expect("parse");
        // The TOML form round-trips, is deterministic, and refuses unknown versions.
        let t = to_toml(&state).expect("toml");
        assert_eq!(parse_toml(&t).expect("parse toml"), state);
        assert_eq!(to_toml(&parse_toml(&t).unwrap()).unwrap(), t, "canonical");
        let text = String::from_utf8(t.clone()).unwrap();
        assert!(text.starts_with("version = 1\n"), "{text}");
        let v2 = text.replacen("version = 1", "version = 2", 1);
        assert!(parse_toml(v2.as_bytes()).is_err());
        assert_eq!(back, state);
        assert_eq!(back.head_index(), Some(0));
        assert!(parse::<State>("state", b"{").is_err());
        // A state written before the redaction epoch existed still reads (epoch 0).
        let mut old: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        old.as_object_mut()
            .expect("object")
            .remove("redactionEpoch");
        let back: State = parse("state", &serde_json::to_vec(&old).expect("json")).expect("old");
        assert_eq!(back.redaction_epoch, 0);
    }
}
