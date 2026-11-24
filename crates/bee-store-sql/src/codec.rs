//! Conversions between store types and column values.

use bee_core::{BeeError, ChangeId, ObjectId, ObjectIdPrefix, Reason, RefName, RepoPath};
use bee_store::{LandedEntry, ObjectState, OrgId, RepoId};

use crate::backend::Value;

/// Length of one encoded landed entry in `ref_log.landed`.
const LANDED_LEN: usize = 16 + 34 + 34;

pub(crate) fn corrupt(what: impl std::fmt::Display) -> BeeError {
    BeeError::new(Reason::Internal, format!("corrupt metadata row: {what}"))
}

/// A `u64` as a column value (`INVALID_ARGUMENT` above `i64::MAX`).
pub(crate) fn int(v: u64) -> Result<i64, BeeError> {
    i64::try_from(v).map_err(|_| {
        BeeError::invalid(
            bee_core::Subcode::InvalidField,
            format!("{v} does not fit in a 64-bit signed column"),
        )
    })
}

/// A `usize` limit, saturating at `i64::MAX`.
pub(crate) fn limit(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(crate) fn oid(id: &ObjectId) -> Value {
    Value::Bytes(id.as_bytes().to_vec())
}

pub(crate) fn parse_oid(b: &[u8]) -> Result<ObjectId, BeeError> {
    ObjectId::from_bytes(b).map_err(|e| corrupt(format!("object id: {e}")))
}

pub(crate) fn parse_opt_oid(b: Option<Vec<u8>>) -> Result<Option<ObjectId>, BeeError> {
    b.map(|b| parse_oid(&b)).transpose()
}

pub(crate) fn parse_change_id(b: &[u8]) -> Result<ChangeId, BeeError> {
    ChangeId::from_bytes(b).map_err(|e| corrupt(format!("change id: {e}")))
}

pub(crate) fn parse_ref(name: &str) -> Result<RefName, BeeError> {
    RefName::new(name).map_err(|e| corrupt(format!("ref name: {e}")))
}

pub(crate) fn ulid_bytes(u: bee_core::Ulid) -> Value {
    Value::Bytes(u.to_bytes().to_vec())
}

fn parse_ulid(b: &[u8]) -> Result<bee_core::Ulid, BeeError> {
    let raw: [u8; 16] = b.try_into().map_err(|_| corrupt("ulid length"))?;
    Ok(bee_core::Ulid::from_bytes(raw))
}

pub(crate) fn parse_org(b: &[u8]) -> Result<OrgId, BeeError> {
    parse_ulid(b).map(OrgId)
}

pub(crate) fn parse_repo(b: &[u8]) -> Result<RepoId, BeeError> {
    parse_ulid(b).map(RepoId)
}

pub(crate) fn state_code(s: ObjectState) -> i64 {
    match s {
        ObjectState::Live => 0,
        ObjectState::Quarantined => 1,
        ObjectState::Purged => 2,
    }
}

pub(crate) fn parse_state(code: i64) -> Result<ObjectState, BeeError> {
    match code {
        0 => Ok(ObjectState::Live),
        1 => Ok(ObjectState::Quarantined),
        2 => Ok(ObjectState::Purged),
        other => Err(corrupt(format!("object state {other}"))),
    }
}

/// `object_paths.path`: empty for the root.
pub(crate) fn path_bytes(p: &Option<RepoPath>) -> Vec<u8> {
    p.as_ref()
        .map(|p| p.as_bytes().to_vec())
        .unwrap_or_default()
}

pub(crate) fn parse_path(b: Vec<u8>) -> Result<Option<RepoPath>, BeeError> {
    if b.is_empty() {
        return Ok(None);
    }
    RepoPath::new(b)
        .map(Some)
        .map_err(|e| corrupt(format!("path: {e}")))
}

pub(crate) fn encode_landed(entries: &[LandedEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * LANDED_LEN);
    for e in entries {
        out.extend_from_slice(e.change_id.as_bytes());
        out.extend_from_slice(e.source.as_bytes());
        out.extend_from_slice(e.result.as_bytes());
    }
    out
}

pub(crate) fn decode_landed(b: &[u8]) -> Result<Vec<LandedEntry>, BeeError> {
    if !b.len().is_multiple_of(LANDED_LEN) {
        return Err(corrupt("ref log landed entries"));
    }
    b.chunks(LANDED_LEN)
        .map(|c| {
            Ok(LandedEntry {
                change_id: parse_change_id(&c[..16])?,
                source: parse_oid(&c[16..50])?,
                result: parse_oid(&c[50..])?,
            })
        })
        .collect()
}

/// The byte range `[lo, hi)` of ids matching a prefix (`hi = None`: unbounded).
pub(crate) fn prefix_range(prefix: &ObjectIdPrefix) -> (Vec<u8>, Option<Vec<u8>>) {
    let kind = prefix.kind();
    let mut lo = vec![kind.algo().as_byte(), kind.as_byte()];
    let digits = prefix.hex().as_bytes();
    let nibble = |c: u8| bee_core::hex::nibble(c).unwrap_or(0);
    for pair in digits.chunks(2) {
        match pair {
            [a, b] => lo.push((nibble(*a) << 4) | nibble(*b)),
            [a] => lo.push(nibble(*a) << 4),
            _ => {}
        }
    }
    // The upper bound: the prefix incremented at its last significant nibble.
    let mut hi = lo.clone();
    let step: u16 = if digits.len() % 2 == 1 { 0x10 } else { 0x01 };
    let mut carry = step;
    for byte in hi.iter_mut().rev() {
        let sum = u16::from(*byte) + carry;
        *byte = (sum & 0xff) as u8;
        carry = sum >> 8;
        if carry == 0 {
            break;
        }
    }
    if carry != 0 {
        return (lo, None);
    }
    (lo, Some(hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;

    #[test]
    fn landed_round_trips() {
        let id = bee_object::hash_id(ObjectKind::Changeset, b"x");
        let e = vec![
            LandedEntry {
                change_id: ChangeId::new([7; 16]),
                source: id,
                result: id,
            };
            3
        ];
        assert_eq!(decode_landed(&encode_landed(&e)).unwrap(), e);
        assert!(decode_landed(&[1, 2, 3]).is_err());
        assert!(decode_landed(&[]).unwrap().is_empty());
    }

    #[test]
    fn prefix_ranges_are_exact() {
        let id = bee_object::hash_id(ObjectKind::Changeset, b"prefix");
        let text = id.to_text();
        let (tag, hex) = text.split_once('_').unwrap();
        for n in 10..=20 {
            let p = ObjectIdPrefix::parse(&format!("{tag}_{}", &hex[..n])).unwrap();
            let (lo, hi) = prefix_range(&p);
            let b = id.as_bytes().to_vec();
            assert!(b >= lo, "{n}");
            assert!(hi.as_ref().is_none_or(|h| b < *h), "{n}");
        }
        let all_f = ObjectIdPrefix::parse(&format!("{tag}_{}", "f".repeat(11))).unwrap();
        let (lo, hi) = prefix_range(&all_f);
        assert_eq!(lo.len(), 2 + 6);
        let hi = hi.unwrap();
        assert_eq!(hi[1], lo[1] + 1);
        assert!(int(u64::MAX).is_err());
        assert_eq!(limit(usize::MAX), i64::MAX);
        for s in [
            ObjectState::Live,
            ObjectState::Quarantined,
            ObjectState::Purged,
        ] {
            assert_eq!(parse_state(state_code(s)).unwrap(), s);
        }
        assert!(parse_state(9).is_err());
        assert_eq!(parse_path(Vec::new()).unwrap(), None);
    }
}
