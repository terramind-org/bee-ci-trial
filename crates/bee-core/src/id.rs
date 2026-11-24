//! Object ids (SPEC §3.1): 34 bytes of algorithm, kind and digest, with a text form.

use std::fmt;
use std::str::FromStr;

use crate::error::{BeeError, Reason, Result, Subcode};
use crate::hex;

/// Length of an object id in bytes.
pub const OBJECT_ID_LEN: usize = 34;
/// Length of an object id's digest in bytes.
pub const DIGEST_LEN: usize = 32;
/// Minimum hex digits in an abbreviated id (SPEC §3.1).
pub const MIN_PREFIX_HEX: usize = 10;

/// The algorithm byte of an object id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HashAlgo {
    /// `0x01`: BLAKE3-256. Every bee object uses it.
    Blake3,
    /// `0x02`: a foreign git object id, only in gitlink tree entries (SPEC §4.2), always
    /// with kind [`ObjectKind::Foreign`].
    Git,
}

impl HashAlgo {
    /// The algorithm byte.
    pub fn as_byte(self) -> u8 {
        match self {
            HashAlgo::Blake3 => 0x01,
            HashAlgo::Git => 0x02,
        }
    }

    /// Parses an algorithm byte; reserved values give `None`.
    pub fn from_byte(b: u8) -> Option<HashAlgo> {
        match b {
            0x01 => Some(HashAlgo::Blake3),
            0x02 => Some(HashAlgo::Git),
            _ => None,
        }
    }

    fn digit(self) -> char {
        match self {
            HashAlgo::Blake3 => '1',
            HashAlgo::Git => '2',
        }
    }
}

/// The kind byte of an object id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    /// `0x00`: a foreign object (git commit of a submodule); only with [`HashAlgo::Git`].
    Foreign,
    /// `0x01`: raw file bytes.
    Blob,
    /// `0x02`: a directory listing.
    Tree,
    /// `0x03`: a changeset.
    Changeset,
    /// `0x04`: changeset metadata.
    Meta,
    /// `0x10`–`0x7F`: a derived object kind.
    Derived(u8),
}

impl ObjectKind {
    /// The kind byte.
    pub fn as_byte(self) -> u8 {
        match self {
            ObjectKind::Foreign => 0x00,
            ObjectKind::Blob => 0x01,
            ObjectKind::Tree => 0x02,
            ObjectKind::Changeset => 0x03,
            ObjectKind::Meta => 0x04,
            ObjectKind::Derived(b) => b,
        }
    }

    /// Parses a kind byte; reserved values give `None`.
    pub fn from_byte(b: u8) -> Option<ObjectKind> {
        match b {
            0x00 => Some(ObjectKind::Foreign),
            0x01 => Some(ObjectKind::Blob),
            0x02 => Some(ObjectKind::Tree),
            0x03 => Some(ObjectKind::Changeset),
            0x04 => Some(ObjectKind::Meta),
            0x10..=0x7f => Some(ObjectKind::Derived(b)),
            _ => None,
        }
    }

    /// A derived kind, if `b` is in the derived range `0x10`–`0x7F`.
    pub fn derived(b: u8) -> Option<ObjectKind> {
        match b {
            0x10..=0x7f => Some(ObjectKind::Derived(b)),
            _ => None,
        }
    }

    /// The algorithm this kind is used with.
    pub fn algo(self) -> HashAlgo {
        match self {
            ObjectKind::Foreign => HashAlgo::Git,
            _ => HashAlgo::Blake3,
        }
    }

    /// The text-form tag before `_`: letter, algorithm digit and, for derived kinds, two hex
    /// digits of the kind byte (`b1`, `t1`, `c1`, `m1`, `d110`, `g2`).
    pub fn text_tag(self) -> String {
        let algo = self.algo().digit();
        match self {
            ObjectKind::Foreign => format!("g{algo}"),
            ObjectKind::Blob => format!("b{algo}"),
            ObjectKind::Tree => format!("t{algo}"),
            ObjectKind::Changeset => format!("c{algo}"),
            ObjectKind::Meta => format!("m{algo}"),
            ObjectKind::Derived(b) => format!("d{algo}{b:02x}"),
        }
    }

    fn from_text_tag(tag: &str) -> Option<ObjectKind> {
        let kind = match tag {
            "g2" => ObjectKind::Foreign,
            "b1" => ObjectKind::Blob,
            "t1" => ObjectKind::Tree,
            "c1" => ObjectKind::Changeset,
            "m1" => ObjectKind::Meta,
            _ => {
                let rest = tag.strip_prefix("d1")?;
                if rest.len() != 2 {
                    return None;
                }
                let b = hex::decode(rest)?;
                ObjectKind::derived(b[0])?
            }
        };
        Some(kind)
    }
}

/// A 34-byte object id: algorithm byte, kind byte, 32-byte digest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; OBJECT_ID_LEN]);

impl ObjectId {
    /// Builds a BLAKE3 id of `kind` from a digest. For [`ObjectKind::Foreign`] use
    /// [`ObjectId::git_foreign`].
    pub fn new(kind: ObjectKind, digest: [u8; DIGEST_LEN]) -> ObjectId {
        let mut raw = [0u8; OBJECT_ID_LEN];
        raw[0] = kind.algo().as_byte();
        raw[1] = kind.as_byte();
        raw[2..].copy_from_slice(&digest);
        ObjectId(raw)
    }

    /// A gitlink's foreign id: `0x02 0x00` then a 20-byte (SHA-1) or 32-byte (SHA-256) git
    /// object id, zero-padded to 32 bytes (SPEC §4.2).
    pub fn git_foreign(git_oid: &[u8]) -> Result<ObjectId> {
        if git_oid.len() != 20 && git_oid.len() != 32 {
            return Err(BeeError::invalid(
                Subcode::InvalidId,
                format!(
                    "git object id must be 20 or 32 bytes, got {}",
                    git_oid.len()
                ),
            ));
        }
        let mut digest = [0u8; DIGEST_LEN];
        digest[..git_oid.len()].copy_from_slice(git_oid);
        Ok(ObjectId::new(ObjectKind::Foreign, digest))
    }

    /// Parses the raw 34-byte form, rejecting reserved algorithm or kind bytes and
    /// algorithm/kind pairs that do not go together.
    pub fn from_bytes(bytes: &[u8]) -> Result<ObjectId> {
        let raw: [u8; OBJECT_ID_LEN] = bytes.try_into().map_err(|_| {
            BeeError::invalid(
                Subcode::InvalidId,
                format!(
                    "object id must be {OBJECT_ID_LEN} bytes, got {}",
                    bytes.len()
                ),
            )
        })?;
        let algo = HashAlgo::from_byte(raw[0]).ok_or_else(|| {
            BeeError::invalid(
                Subcode::InvalidId,
                format!("reserved algorithm byte {:#04x}", raw[0]),
            )
        })?;
        let kind = ObjectKind::from_byte(raw[1]).ok_or_else(|| {
            BeeError::invalid(
                Subcode::InvalidId,
                format!("reserved kind byte {:#04x}", raw[1]),
            )
        })?;
        if kind.algo() != algo {
            return Err(BeeError::invalid(
                Subcode::InvalidId,
                format!("kind {:#04x} cannot use algorithm {:#04x}", raw[1], raw[0]),
            ));
        }
        Ok(ObjectId(raw))
    }

    /// The raw 34 bytes.
    pub fn as_bytes(&self) -> &[u8; OBJECT_ID_LEN] {
        &self.0
    }

    /// The algorithm.
    pub fn algo(&self) -> HashAlgo {
        self.kind().algo()
    }

    /// The kind.
    pub fn kind(&self) -> ObjectKind {
        // Construction guarantees a valid kind byte.
        ObjectKind::from_byte(self.0[1]).unwrap_or(ObjectKind::Foreign)
    }

    /// The 32-byte digest.
    pub fn digest(&self) -> &[u8; DIGEST_LEN] {
        self.0[2..].try_into().unwrap_or(&[0; DIGEST_LEN])
    }

    /// The text form, e.g. `c1_9f3a…` (64 hex digits).
    pub fn to_text(&self) -> String {
        format!("{}_{}", self.kind().text_tag(), hex::encode(self.digest()))
    }

    /// Parses the text form.
    pub fn parse(text: &str) -> Result<ObjectId> {
        let bad = || BeeError::invalid(Subcode::InvalidId, format!("not an object id: {text:?}"));
        let (tag, digest) = text.split_once('_').ok_or_else(bad)?;
        let kind = ObjectKind::from_text_tag(tag).ok_or_else(bad)?;
        if digest.len() != DIGEST_LEN * 2 {
            return Err(bad());
        }
        let bytes = hex::decode(digest).ok_or_else(bad)?;
        let mut d = [0u8; DIGEST_LEN];
        d.copy_from_slice(&bytes);
        Ok(ObjectId::new(kind, d))
    }

    /// Fails with `INVALID_ARGUMENT`/`WRONG_KIND` unless this id has `kind`.
    pub fn expect_kind(&self, kind: ObjectKind) -> Result<()> {
        if self.kind() == kind {
            Ok(())
        } else {
            Err(BeeError::invalid(
                Subcode::WrongKind,
                format!("expected a {} id, got {}", kind.text_tag(), self),
            ))
        }
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl FromStr for ObjectId {
    type Err = BeeError;
    fn from_str(s: &str) -> Result<Self> {
        ObjectId::parse(s)
    }
}

/// An abbreviated object id: tag plus at least [`MIN_PREFIX_HEX`] lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectIdPrefix {
    kind: ObjectKind,
    hex: String,
}

impl ObjectIdPrefix {
    /// Parses `<tag>_<hex>` with 10 to 64 lowercase hex digits.
    pub fn parse(text: &str) -> Result<ObjectIdPrefix> {
        let bad = || BeeError::invalid(Subcode::InvalidId, format!("not an id prefix: {text:?}"));
        let (tag, digits) = text.split_once('_').ok_or_else(bad)?;
        let kind = ObjectKind::from_text_tag(tag).ok_or_else(bad)?;
        if digits.len() < MIN_PREFIX_HEX
            || digits.len() > DIGEST_LEN * 2
            || !hex::is_lower_hex(digits)
        {
            return Err(bad());
        }
        Ok(ObjectIdPrefix {
            kind,
            hex: digits.to_owned(),
        })
    }

    /// The kind the prefix names.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// The hex digits.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// True when `id` has this kind and its digest starts with these digits.
    pub fn matches(&self, id: &ObjectId) -> bool {
        id.kind() == self.kind && hex::encode(id.digest()).starts_with(&self.hex)
    }

    /// Resolves the prefix against candidate ids: exactly one distinct match is returned;
    /// none is `NOT_FOUND`; several are `AMBIGUOUS_PREFIX`, with the sorted matches in the
    /// payload under `candidates`.
    pub fn resolve<'a, I>(&self, candidates: I) -> Result<ObjectId>
    where
        I: IntoIterator<Item = &'a ObjectId>,
    {
        let mut found: Vec<ObjectId> = candidates
            .into_iter()
            .filter(|id| self.matches(id))
            .copied()
            .collect();
        found.sort();
        found.dedup();
        match found.len() {
            0 => Err(BeeError::new(
                Reason::NotFound,
                format!("no object matches {self}"),
            )),
            1 => Ok(found[0]),
            n => Err(BeeError::new(
                Reason::AmbiguousPrefix,
                format!("{self} matches {n} objects"),
            )
            .with_payload(
                "candidates",
                crate::error::PayloadValue::List(
                    found.iter().map(|id| id.to_text().into()).collect(),
                ),
            )),
        }
    }
}

impl fmt::Display for ObjectIdPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.kind.text_tag(), self.hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn digest(seed: u8) -> [u8; 32] {
        let mut d = [0u8; 32];
        for (i, b) in d.iter_mut().enumerate() {
            *b = seed.wrapping_mul(31).wrapping_add(i as u8);
        }
        d
    }

    #[test]
    fn text_form_examples() {
        let id = ObjectId::new(ObjectKind::Changeset, [0xab; 32]);
        assert_eq!(id.to_text(), format!("c1_{}", "ab".repeat(32)));
        assert_eq!(id.as_bytes()[0], 0x01);
        assert_eq!(id.as_bytes()[1], 0x03);
        assert_eq!(id.algo(), HashAlgo::Blake3);
        assert_eq!(ObjectId::parse(&id.to_text()).unwrap(), id);
        assert_eq!(format!("{id:?}"), format!("ObjectId({id})"));
        assert_eq!(id.to_string().parse::<ObjectId>().unwrap(), id);
        for (kind, tag) in [
            (ObjectKind::Blob, "b1"),
            (ObjectKind::Tree, "t1"),
            (ObjectKind::Meta, "m1"),
            (ObjectKind::Foreign, "g2"),
        ] {
            assert_eq!(kind.text_tag(), tag);
        }
    }

    #[test]
    fn derived_kind_text_form() {
        let k = ObjectKind::derived(0x10).unwrap();
        let id = ObjectId::new(k, [1; 32]);
        assert!(id.to_text().starts_with("d110_"));
        assert_eq!(
            ObjectId::parse(&id.to_text()).unwrap().kind(),
            ObjectKind::Derived(0x10)
        );
        let top = ObjectId::new(ObjectKind::Derived(0x7f), [1; 32]);
        assert!(top.to_text().starts_with("d17f_"));
        assert_eq!(ObjectKind::derived(0x0f), None);
        assert_eq!(ObjectKind::derived(0x80), None);
        assert!(ObjectId::parse(&format!("d109_{}", "00".repeat(32))).is_err());
        assert!(ObjectId::parse(&format!("d1_{}", "00".repeat(32))).is_err());
        assert!(ObjectId::parse(&format!("d1100_{}", "00".repeat(32))).is_err());
    }

    #[test]
    fn git_foreign_ids() {
        let sha1 = [0x11u8; 20];
        let id = ObjectId::git_foreign(&sha1).unwrap();
        assert_eq!(id.as_bytes()[0], 0x02);
        assert_eq!(id.as_bytes()[1], 0x00);
        assert_eq!(&id.digest()[..20], &sha1);
        assert_eq!(&id.digest()[20..], &[0u8; 12]);
        assert_eq!(id.kind(), ObjectKind::Foreign);
        assert_eq!(id.algo(), HashAlgo::Git);
        assert!(id.to_text().starts_with("g2_"));
        assert_eq!(ObjectId::parse(&id.to_text()).unwrap(), id);
        assert!(ObjectId::git_foreign(&[0u8; 32]).is_ok());
        assert!(
            ObjectId::git_foreign(&[0u8; 21])
                .unwrap_err()
                .is_invalid(Subcode::InvalidId)
        );
    }

    #[test]
    fn from_bytes_validation() {
        let id = ObjectId::new(ObjectKind::Tree, digest(3));
        assert_eq!(ObjectId::from_bytes(id.as_bytes()).unwrap(), id);
        assert!(ObjectId::from_bytes(&[1; 33]).is_err());
        let mut raw = *id.as_bytes();
        raw[0] = 0x03;
        assert!(ObjectId::from_bytes(&raw).is_err(), "reserved algorithm");
        raw[0] = 0x01;
        raw[1] = 0x05;
        assert!(ObjectId::from_bytes(&raw).is_err(), "reserved kind");
        raw[1] = 0x80;
        assert!(ObjectId::from_bytes(&raw).is_err(), "above derived range");
        raw[1] = 0x00;
        assert!(
            ObjectId::from_bytes(&raw).is_err(),
            "foreign kind with blake3"
        );
        raw[0] = 0x02;
        raw[1] = 0x01;
        assert!(
            ObjectId::from_bytes(&raw).is_err(),
            "git algo with blob kind"
        );
        assert_eq!(HashAlgo::from_byte(0), None);
        assert_eq!(ObjectKind::from_byte(0x0f), None);
    }

    #[test]
    fn parse_rejects_malformed_text() {
        let good = format!("c1_{}", "0".repeat(64));
        assert!(ObjectId::parse(&good).is_ok());
        for bad in [
            String::new(),
            "c1".into(),
            format!("c1_{}", "0".repeat(63)),
            format!("c1_{}", "0".repeat(65)),
            format!("C1_{}", "0".repeat(64)),
            format!("c1_{}", "A".repeat(64)),
            format!("c2_{}", "0".repeat(64)),
            format!("x1_{}", "0".repeat(64)),
            format!("g1_{}", "0".repeat(64)),
            format!("c1-{}", "0".repeat(64)),
        ] {
            let err = ObjectId::parse(&bad).unwrap_err();
            assert!(err.is_invalid(Subcode::InvalidId), "{bad}");
        }
    }

    #[test]
    fn expect_kind_reports_wrong_kind() {
        let id = ObjectId::new(ObjectKind::Blob, [0; 32]);
        assert!(id.expect_kind(ObjectKind::Blob).is_ok());
        assert!(
            id.expect_kind(ObjectKind::Tree)
                .unwrap_err()
                .is_invalid(Subcode::WrongKind)
        );
    }

    #[test]
    fn prefix_parse_and_match() {
        let id = ObjectId::new(ObjectKind::Changeset, digest(7));
        let text = id.to_text();
        let p = ObjectIdPrefix::parse(&text[..13]).unwrap();
        assert_eq!(p.kind(), ObjectKind::Changeset);
        assert_eq!(p.hex().len(), 10);
        assert!(p.matches(&id));
        assert_eq!(p.to_string(), &text[..13]);
        assert!(
            ObjectIdPrefix::parse(&text).is_ok(),
            "a full id is a prefix"
        );
        assert!(
            ObjectIdPrefix::parse(&text[..12]).is_err(),
            "9 digits is too short"
        );
        assert!(ObjectIdPrefix::parse("c1_ABCDEF0123").is_err());
        assert!(ObjectIdPrefix::parse("c1abcdef0123").is_err());
        let tree = ObjectId::new(ObjectKind::Tree, digest(7));
        assert!(!p.matches(&tree), "kind must match");
        let odd = ObjectIdPrefix::parse(&text[..14]).unwrap();
        assert!(odd.matches(&id), "odd digit counts match by nibble");
    }

    #[test]
    fn ambiguous_prefix_is_reported() {
        let mut d1 = [0u8; 32];
        let mut d2 = [0u8; 32];
        d1[31] = 1;
        d2[31] = 2;
        let a = ObjectId::new(ObjectKind::Changeset, d1);
        let b = ObjectId::new(ObjectKind::Changeset, d2);
        let p = ObjectIdPrefix::parse("c1_0000000000").unwrap();
        let err = p.resolve([&a, &b]).unwrap_err();
        assert_eq!(err.reason, Reason::AmbiguousPrefix);
        match err.payload.get("candidates") {
            Some(crate::error::PayloadValue::List(items)) => assert_eq!(items.len(), 2),
            other => panic!("unexpected payload {other:?}"),
        }
        assert_eq!(p.resolve([&a, &a]).unwrap(), a, "duplicates are one match");
        let p2 = ObjectIdPrefix::parse(&a.to_text()).unwrap();
        assert_eq!(p2.resolve([&a, &b]).unwrap(), a);
        let none = ObjectIdPrefix::parse("c1_ffffffffff").unwrap();
        assert_eq!(none.resolve([&a, &b]).unwrap_err().reason, Reason::NotFound);
    }

    proptest! {
        #[test]
        fn text_and_bytes_round_trip(d in proptest::array::uniform32(any::<u8>()), k in 0u8..=0x7f) {
            if let Some(kind) = ObjectKind::from_byte(k) {
                let id = if kind == ObjectKind::Foreign {
                    ObjectId::git_foreign(&d).unwrap()
                } else {
                    ObjectId::new(kind, d)
                };
                prop_assert_eq!(ObjectId::parse(&id.to_text()).unwrap(), id);
                prop_assert_eq!(ObjectId::from_bytes(id.as_bytes()).unwrap(), id);
                prop_assert_eq!(id.digest(), &d);
            }
        }
    }
}
