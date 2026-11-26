//! Git object formats and object ids.
//!
//! A repository uses exactly one object format for its whole life: SHA-1 (with collision
//! detection, so SHAttered-style inputs are rejected rather than silently aliased) or
//! SHA-256. Nothing here converts between the two.

use std::fmt;

use bee_core::{BeeError, ObjectId, ObjectKind, Result, Subcode, hex};
use sha1collisiondetection::Sha1CD;
use sha2::{Digest, Sha256};

use crate::error::integrity;
use crate::object::ObjectType;

/// A git object format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectFormat {
    /// SHA-1, 20-byte ids. Hashing detects cryptanalytic collision attacks.
    Sha1,
    /// SHA-256, 32-byte ids.
    Sha256,
}

impl ObjectFormat {
    /// Both formats.
    pub const ALL: [ObjectFormat; 2] = [ObjectFormat::Sha1, ObjectFormat::Sha256];

    /// Length of an object id in bytes: 20 or 32.
    pub fn oid_len(self) -> usize {
        match self {
            ObjectFormat::Sha1 => 20,
            ObjectFormat::Sha256 => 32,
        }
    }

    /// The name git uses (`extensions.objectFormat`): `sha1` or `sha256`.
    pub fn name(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }

    /// Parses `sha1` or `sha256`.
    pub fn from_name(name: &str) -> Option<ObjectFormat> {
        match name {
            "sha1" => Some(ObjectFormat::Sha1),
            "sha256" => Some(ObjectFormat::Sha256),
            _ => None,
        }
    }

    /// A fresh hasher.
    pub fn hasher(self) -> GitHasher {
        GitHasher::new(self)
    }

    /// The hash of raw bytes (pack and index checksums use this).
    pub fn hash(self, data: &[u8]) -> Result<GitOid> {
        let mut h = self.hasher();
        h.update(data);
        h.finish()
    }

    /// The id of an object: the hash of `"<type> <len>\0"` followed by the body.
    pub fn hash_object(self, kind: ObjectType, body: &[u8]) -> Result<GitOid> {
        let mut h = self.hasher();
        h.update(&crate::object::header(kind, body.len()));
        h.update(body);
        h.finish()
    }

    /// The id of the empty tree in this format.
    pub fn empty_tree(self) -> GitOid {
        // Hashing fixed, tiny input cannot trigger collision detection.
        self.hash_object(ObjectType::Tree, b"")
            .unwrap_or(GitOid::null(self))
    }

    /// Wraps raw id bytes, which must be exactly [`ObjectFormat::oid_len`] long.
    pub fn oid_from_bytes(self, bytes: &[u8]) -> Result<GitOid> {
        let oid = GitOid::from_bytes(bytes)?;
        if oid.format() != self {
            return Err(BeeError::invalid(
                Subcode::InvalidId,
                format!(
                    "git id has {} bytes; this repository uses {}",
                    bytes.len(),
                    self.name()
                ),
            ));
        }
        Ok(oid)
    }

    /// Parses lowercase hex of exactly `2 * oid_len` digits.
    pub fn parse_hex(self, text: &[u8]) -> Result<GitOid> {
        let bad = || {
            BeeError::invalid(
                Subcode::InvalidId,
                format!(
                    "not a {} git id: {:?}",
                    self.name(),
                    String::from_utf8_lossy(text)
                ),
            )
        };
        if text.len() != self.oid_len() * 2 {
            return Err(bad());
        }
        let text = std::str::from_utf8(text).map_err(|_| bad())?;
        let bytes = hex::decode(text).ok_or_else(bad)?;
        self.oid_from_bytes(&bytes)
    }
}

/// A git object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitOid {
    /// A SHA-1 id.
    Sha1([u8; 20]),
    /// A SHA-256 id.
    Sha256([u8; 32]),
}

impl GitOid {
    /// The all-zero id of a format (git's "null oid").
    pub fn null(format: ObjectFormat) -> GitOid {
        match format {
            ObjectFormat::Sha1 => GitOid::Sha1([0; 20]),
            ObjectFormat::Sha256 => GitOid::Sha256([0; 32]),
        }
    }

    /// Wraps 20 (SHA-1) or 32 (SHA-256) raw bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<GitOid> {
        if let Ok(b) = <[u8; 20]>::try_from(bytes) {
            Ok(GitOid::Sha1(b))
        } else if let Ok(b) = <[u8; 32]>::try_from(bytes) {
            Ok(GitOid::Sha256(b))
        } else {
            Err(BeeError::invalid(
                Subcode::InvalidId,
                format!("git id must be 20 or 32 bytes, got {}", bytes.len()),
            ))
        }
    }

    /// Parses lowercase hex of 40 or 64 digits.
    pub fn parse(text: &str) -> Result<GitOid> {
        let format = if text.len() == 64 {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        };
        format.parse_hex(text.as_bytes())
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            GitOid::Sha1(b) => b,
            GitOid::Sha256(b) => b,
        }
    }

    /// The format this id belongs to.
    pub fn format(&self) -> ObjectFormat {
        match self {
            GitOid::Sha1(_) => ObjectFormat::Sha1,
            GitOid::Sha256(_) => ObjectFormat::Sha256,
        }
    }

    /// Lowercase hex.
    pub fn to_hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    /// True for the all-zero id.
    pub fn is_null(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }

    /// The bee foreign id a gitlink entry carries (SPEC §4.2).
    pub fn to_foreign(&self) -> ObjectId {
        let mut digest = [0u8; 32];
        digest[..self.as_bytes().len()].copy_from_slice(self.as_bytes());
        ObjectId::new(ObjectKind::Foreign, digest)
    }

    /// The git id inside a gitlink's foreign id. For SHA-1 the 12 padding bytes must be zero.
    pub fn from_foreign(id: &ObjectId, format: ObjectFormat) -> Result<GitOid> {
        id.expect_kind(ObjectKind::Foreign)?;
        let digest = id.digest();
        let n = format.oid_len();
        if digest[n..].iter().any(|&b| b != 0) {
            return Err(BeeError::invalid(
                Subcode::InvalidId,
                format!("foreign id {id} is not a padded {} git id", format.name()),
            ));
        }
        format.oid_from_bytes(&digest[..n])
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitOid({self})")
    }
}

/// An incremental hasher for one object format.
#[derive(Clone, Debug)]
pub struct GitHasher(HasherState);

#[derive(Clone, Debug)]
enum HasherState {
    Sha1(Box<Sha1CD>),
    Sha256(Sha256),
}

impl GitHasher {
    /// A fresh hasher.
    pub fn new(format: ObjectFormat) -> GitHasher {
        GitHasher(match format {
            ObjectFormat::Sha1 => HasherState::Sha1(Box::default()),
            ObjectFormat::Sha256 => HasherState::Sha256(Sha256::new()),
        })
    }

    /// Feeds bytes.
    pub fn update(&mut self, data: &[u8]) {
        match &mut self.0 {
            HasherState::Sha1(h) => h.update(data),
            HasherState::Sha256(h) => Digest::update(h, data),
        }
    }

    /// The digest. For SHA-1, input carrying a cryptanalytic collision attack fails with
    /// `INTEGRITY` instead of returning an id another object could share.
    pub fn finish(self) -> Result<GitOid> {
        match self.0 {
            HasherState::Sha1(h) => {
                let out = h.finalize_cd().map_err(|_| {
                    integrity("SHA-1 collision attack detected in hashed git data")
                        .with_payload("collision", true)
                })?;
                GitOid::from_bytes(&out[..])
            }
            HasherState::Sha256(h) => GitOid::from_bytes(&h.finalize()[..]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids() {
        assert_eq!(
            ObjectFormat::Sha1.empty_tree().to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        assert_eq!(
            ObjectFormat::Sha256.empty_tree().to_hex(),
            "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
        );
        assert_eq!(
            ObjectFormat::Sha1
                .hash_object(ObjectType::Blob, b"")
                .unwrap()
                .to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            ObjectFormat::Sha1
                .hash_object(ObjectType::Blob, b"hello\n")
                .unwrap()
                .to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn hex_and_bytes() {
        for format in ObjectFormat::ALL {
            let oid = format.empty_tree();
            assert_eq!(GitOid::parse(&oid.to_hex()).unwrap(), oid);
            assert_eq!(format.parse_hex(oid.to_hex().as_bytes()).unwrap(), oid);
            assert_eq!(format.oid_from_bytes(oid.as_bytes()).unwrap(), oid);
            assert_eq!(oid.format(), format);
            assert!(!oid.is_null());
            assert!(GitOid::null(format).is_null());
            assert_eq!(ObjectFormat::from_name(format.name()), Some(format));
            assert_eq!(format!("{oid:?}"), format!("GitOid({oid})"));
        }
        assert!(GitOid::parse(&"A".repeat(40)).is_err());
        assert!(GitOid::parse(&"0".repeat(39)).is_err());
        assert!(ObjectFormat::Sha256.oid_from_bytes(&[0; 20]).is_err());
        assert!(GitOid::from_bytes(&[0; 21]).is_err());
        assert_eq!(ObjectFormat::from_name("md5"), None);
    }

    #[test]
    fn foreign_round_trip() {
        for format in ObjectFormat::ALL {
            let oid = format.hash(b"x").unwrap();
            let f = oid.to_foreign();
            assert_eq!(f.kind(), ObjectKind::Foreign);
            assert_eq!(GitOid::from_foreign(&f, format).unwrap(), oid);
        }
        let sha256 = ObjectFormat::Sha256.hash(b"x").unwrap().to_foreign();
        assert!(GitOid::from_foreign(&sha256, ObjectFormat::Sha1).is_err());
        assert!(GitOid::from_foreign(&bee_object::blob_id(b""), ObjectFormat::Sha1).is_err());
    }
}
