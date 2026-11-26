//! Conversions between `bee.v1` messages and domain types.

use bee_core::{ChangeId, Mode, ObjectId, RepoPath};
use bee_history::pathhist::LeafEntry;
use bee_history::{ConflictSet, ConflictSide, MergeBase};
use bee_merge::ConflictKind;
use bee_object::{Changeset, DeltaEntry, DeltaOp, LocalCheck, Meta, Provenance, Timestamp};
use bee_proto::v1;

use crate::errors::{ApiError, ApiResult};

/// A required object id field.
pub fn id_from(id: Option<&v1::ObjectId>, what: &str) -> ApiResult<ObjectId> {
    let id = id.ok_or_else(|| ApiError::invalid("missing_field", format!("{what} is required")))?;
    ObjectId::from_bytes(&id.raw).map_err(ApiError::from)
}

/// An optional object id field (absent or empty = `None`).
pub fn opt_id_from(id: Option<&v1::ObjectId>, what: &str) -> ApiResult<Option<ObjectId>> {
    match id {
        Some(i) if !i.raw.is_empty() => id_from(Some(i), what).map(Some),
        _ => Ok(None),
    }
}

/// The wire form of an id.
pub fn id_to(id: &ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// A required ChangeId.
pub fn change_id_from(c: Option<&v1::ChangeId>, what: &str) -> ApiResult<ChangeId> {
    let c = c.ok_or_else(|| ApiError::invalid("missing_field", format!("{what} is required")))?;
    ChangeId::from_bytes(&c.raw).map_err(ApiError::from)
}

/// The wire form of a ChangeId.
pub fn change_id_to(c: &ChangeId) -> v1::ChangeId {
    v1::ChangeId {
        raw: c.as_bytes().to_vec(),
    }
}

/// The wire value of a mode (gitlinks use 5, outside the proto enum's named values).
pub fn mode_to(m: Mode) -> i32 {
    m.as_u64() as i32
}

/// A mode from its wire value.
pub fn mode_from(m: i32) -> ApiResult<Mode> {
    if m <= 0 {
        return Err(ApiError::invalid("invalid_mode", "mode is required"));
    }
    Mode::from_u64(m as u64).map_err(ApiError::from)
}

/// A repository path from bytes (`INVALID_ARGUMENT`/`invalid_path`).
pub fn path_from(bytes: &[u8]) -> ApiResult<RepoPath> {
    RepoPath::new(bytes.to_vec()).map_err(ApiError::from)
}

/// An optional path: empty bytes or `/` mean the root.
pub fn opt_path_from(bytes: &[u8]) -> ApiResult<Option<RepoPath>> {
    let trimmed: &[u8] = {
        let mut b = bytes;
        while let [b'/', rest @ ..] = b {
            b = rest;
        }
        while let [rest @ .., b'/'] = b {
            b = rest;
        }
        b
    };
    if trimmed.is_empty() {
        Ok(None)
    } else {
        path_from(trimmed).map(Some)
    }
}

/// Delta entries from the wire, sorted by path as the encoding requires.
pub fn delta_from(entries: &[v1::DeltaEntry]) -> ApiResult<Vec<DeltaEntry>> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let path = path_from(&e.path)?;
        let op = match &e.op {
            Some(v1::delta_entry::Op::Put(p)) => DeltaOp::Put {
                mode: mode_from(p.mode)?,
                id: id_from(p.blob.as_ref(), "put.blob")?,
                size: p.size,
            },
            Some(v1::delta_entry::Op::Delete(true)) => DeltaOp::Delete,
            _ => {
                return Err(ApiError::invalid(
                    "invalid_delta",
                    format!("delta entry {path} needs a put or delete = true"),
                ));
            }
        };
        out.push(DeltaEntry { path, op });
    }
    out.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(out)
}

/// One delta entry on the wire.
pub fn delta_entry_to(e: &DeltaEntry) -> v1::DeltaEntry {
    v1::DeltaEntry {
        path: e.path.as_bytes().to_vec(),
        op: Some(match e.op {
            DeltaOp::Delete => v1::delta_entry::Op::Delete(true),
            DeltaOp::Put { mode, id, size } => v1::delta_entry::Op::Put(v1::Put {
                mode: mode_to(mode),
                blob: Some(id_to(&id)),
                size,
            }),
        }),
    }
}

/// A signature from author/committer fields.
pub fn signature_to(name: &str, t: &Timestamp) -> v1::Signature {
    v1::Signature {
        name: name.to_string(),
        time: Some(pbjson_types::Timestamp {
            seconds: t.seconds,
            nanos: 0,
        }),
        tz_offset_minutes: t.tz_offset_minutes,
    }
}

/// A timestamp from a wire signature (`None` when absent).
pub fn timestamp_from(s: Option<&v1::Signature>) -> Option<Timestamp> {
    s.and_then(|s| {
        s.time
            .as_ref()
            .map(|t| Timestamp::new(t.seconds, s.tz_offset_minutes))
    })
}

/// Provenance from the wire (the server sets `principal_chain` separately).
pub fn provenance_from(p: &v1::Provenance) -> ApiResult<Provenance> {
    let prompt_digest = if p.prompt_digest.is_empty() {
        None
    } else {
        Some(
            <[u8; 32]>::try_from(p.prompt_digest.as_slice()).map_err(|_| {
                ApiError::invalid("invalid_field", "provenance.prompt_digest must be 32 bytes")
            })?,
        )
    };
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    Ok(Provenance {
        principal_chain: p.principal_chain.clone(),
        runtime: opt(&p.runtime),
        model: opt(&p.model),
        session: opt(&p.session),
        prompt_digest,
        checks_run: p
            .checks_run
            .iter()
            .map(|c| LocalCheck {
                name: c.name.clone(),
                result: c.result.clone(),
                duration_ms: c.duration_ms,
            })
            .collect(),
        cost: p.cost.as_ref().map(|c| bee_object::Cost {
            input_tokens: (c.input_tokens > 0).then_some(c.input_tokens),
            output_tokens: (c.output_tokens > 0).then_some(c.output_tokens),
            usd_micros: (c.usd_micros > 0).then_some(c.usd_micros),
        }),
    })
}

/// Provenance on the wire.
pub fn provenance_to(p: &Provenance) -> v1::Provenance {
    v1::Provenance {
        principal_chain: p.principal_chain.clone(),
        runtime: p.runtime.clone().unwrap_or_default(),
        model: p.model.clone().unwrap_or_default(),
        session: p.session.clone().unwrap_or_default(),
        prompt_digest: p.prompt_digest.map(|d| d.to_vec()).unwrap_or_default(),
        checks_run: p
            .checks_run
            .iter()
            .map(|c| v1::LocalCheck {
                name: c.name.clone(),
                result: c.result.clone(),
                duration_ms: c.duration_ms,
            })
            .collect(),
        cost: p.cost.map(|c| v1::Cost {
            input_tokens: c.input_tokens.unwrap_or(0),
            output_tokens: c.output_tokens.unwrap_or(0),
            usd_micros: c.usd_micros.unwrap_or(0),
        }),
    }
}

/// A meta on the wire.
pub fn meta_to(m: &Meta) -> v1::Meta {
    v1::Meta {
        author: m.author.clone(),
        author_time: Some(signature_to(&m.author, &m.author_time)),
        committer: m.committer.clone().unwrap_or_default(),
        committer_time: m
            .committer_time
            .map(|t| signature_to(m.committer.as_deref().unwrap_or(""), &t)),
        creator: m.creator.clone(),
        message: m.message.clone(),
        provenance: m.provenance.as_ref().map(provenance_to),
        extensions: m
            .git
            .iter()
            .map(|g| v1::MetaExtension {
                key: bee_object::GIT_EXTENSION_KEY as u32,
                value: g.clone(),
            })
            .collect(),
    }
}

/// A changeset on the wire. `delta` may be a filtered view (then `filtered` is set).
pub fn changeset_to(
    id: &ObjectId,
    cs: &Changeset,
    delta: &[DeltaEntry],
    filtered: bool,
    meta: Option<&Meta>,
    generation: u64,
) -> v1::Changeset {
    v1::Changeset {
        id: Some(id_to(id)),
        parents: cs.parents().iter().map(id_to).collect(),
        root: Some(id_to(cs.root())),
        meta_id: Some(id_to(cs.meta())),
        change_id: Some(change_id_to(cs.change_id())),
        delta: delta.iter().map(delta_entry_to).collect(),
        meta: meta.map(meta_to),
        filtered,
        generation,
    }
}

/// The wire conflict kind.
pub fn conflict_kind_to(k: ConflictKind) -> i32 {
    use v1::ConflictKind as K;
    (match k {
        ConflictKind::Content | ConflictKind::RenameRename => K::Content,
        ConflictKind::AddAdd => K::AddAdd,
        ConflictKind::ModifyDelete => K::ModifyDelete,
        ConflictKind::Symlink => K::Symlink,
        ConflictKind::Binary => K::Binary,
        ConflictKind::Redacted => K::Redacted,
        ConflictKind::Mode => K::Mode,
        ConflictKind::FileDirectory => K::FileDirectory,
    }) as i32
}

fn side_to(s: &Option<ConflictSide>) -> Option<v1::ConflictSide> {
    s.as_ref().map(|s| v1::ConflictSide {
        mode: mode_to(s.mode),
        blob: s.id.as_ref().map(id_to),
    })
}

/// A conflict set on the wire (SPEC §7.3.2), with the two sides that were merged.
pub fn conflict_set_to(c: &ConflictSet, ours: &ObjectId, theirs: &ObjectId) -> v1::ConflictSet {
    v1::ConflictSet {
        conflicts: c
            .conflicts
            .iter()
            .map(|p| v1::Conflict {
                path: p.path.as_bytes().to_vec(),
                kind: conflict_kind_to(p.kind),
                base: side_to(&p.base),
                ours: side_to(&p.ours),
                theirs: side_to(&p.theirs),
                hunks: p
                    .hunks
                    .iter()
                    .map(|h| v1::ConflictHunk {
                        base_start: h.base_start as u32,
                        base_len: h.base_len as u32,
                        ours_start: h.ours_start as u32,
                        ours_len: h.ours_len as u32,
                        theirs_start: h.theirs_start as u32,
                        theirs_len: h.theirs_len as u32,
                    })
                    .collect(),
            })
            .collect(),
        clean_entries: c
            .resolved
            .iter()
            .map(|(path, leaf)| v1::DeltaEntry {
                path: path.as_bytes().to_vec(),
                op: Some(match leaf {
                    None => v1::delta_entry::Op::Delete(true),
                    Some(LeafEntry { mode, id, size }) => v1::delta_entry::Op::Put(v1::Put {
                        mode: mode_to(*mode),
                        blob: Some(id_to(id)),
                        size: *size,
                    }),
                }),
            })
            .collect(),
        base: match &c.base {
            MergeBase::Unique(b) => Some(id_to(b)),
            _ => None,
        },
        ours: Some(id_to(ours)),
        theirs: Some(id_to(theirs)),
    }
}

/// `MERGE_CONFLICT` carrying the conflicts as data.
pub fn conflict_error(c: &ConflictSet, ours: &ObjectId, theirs: &ObjectId) -> ApiError {
    let mut e = ApiError::new(
        v1::ErrorReason::MergeConflict,
        format!("{} conflicting path(s)", c.conflicts.len()),
    );
    e.detail.payload = Some(v1::error_detail::Payload::Conflicts(conflict_set_to(
        c, ours, theirs,
    )));
    e
}

/// A rebase conflict with the identifiers needed to create and land its resolution.
pub fn rebase_conflict_error(
    c: &ConflictSet,
    onto: &ObjectId,
    source: &ObjectId,
    change_id: &ChangeId,
) -> ApiError {
    let mut e = conflict_error(c, onto, source);
    let paths = c
        .conflicts
        .iter()
        .take(3)
        .map(|conflict| conflict.path.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let change_id = change_id.to_text();
    e.detail.message = format!(
        "{} conflicting path(s): {paths}. Rebase onto {} conflicted while keeping change {change_id}. Create a resolved changeset with {} as its parent and {change_id} as its change id, then land it.",
        c.conflicts.len(),
        onto.to_text(),
        onto.to_text(),
    );
    e
}

/// A ref on the wire.
pub fn ref_to(name: &str, target: Option<&ObjectId>, version: u64) -> v1::Ref {
    v1::Ref {
        name: name.to_string(),
        target: target.map(id_to),
        version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_round_trips_and_sorts() {
        let blob = bee_object::blob_id(b"x");
        let wire = vec![
            delta_entry_to(&DeltaEntry::put(
                RepoPath::new("b").unwrap(),
                Mode::File,
                blob,
                1,
            )),
            delta_entry_to(&DeltaEntry::delete(RepoPath::new("a").unwrap())),
        ];
        let back = delta_from(&wire).unwrap();
        assert_eq!(back[0].path.as_bytes(), b"a");
        assert_eq!(
            back[1].op,
            DeltaOp::Put {
                mode: Mode::File,
                id: blob,
                size: 1
            }
        );
        let bad = v1::DeltaEntry {
            path: b"x".to_vec(),
            op: None,
        };
        assert_eq!(
            delta_from(&[bad]).unwrap_err().detail.subcode,
            "invalid_delta"
        );
    }

    #[test]
    fn ids_and_paths() {
        let id = bee_object::blob_id(b"y");
        assert_eq!(id_from(Some(&id_to(&id)), "id").unwrap(), id);
        assert!(id_from(None, "id").is_err());
        assert_eq!(opt_path_from(b"/").unwrap(), None);
        assert_eq!(opt_path_from(b"/a/b/").unwrap().unwrap().as_bytes(), b"a/b");
        assert!(mode_from(0).is_err());
        assert_eq!(mode_from(5).unwrap(), Mode::Gitlink);
    }

    #[test]
    fn provenance_round_trips() {
        let p = v1::Provenance {
            principal_chain: vec!["A".into()],
            runtime: "claude-code".into(),
            prompt_digest: vec![7; 32],
            ..Default::default()
        };
        let d = provenance_from(&p).unwrap();
        assert_eq!(provenance_to(&d), p);
        let bad = v1::Provenance {
            prompt_digest: vec![1; 3],
            ..Default::default()
        };
        assert!(provenance_from(&bad).is_err());
    }
}
