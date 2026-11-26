//! Object types, raw objects and the loose object encoding.
//!
//! A loose object is `zlib("<type> <len>\0" ‖ body)`. The zlib level is not part of identity;
//! only the inflated bytes are hashed.

use std::io::{Read, Write};

use bee_core::Result;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::commit::GitCommit;
use crate::error::malformed;
use crate::oid::{GitOid, ObjectFormat};
use crate::tag::GitTag;
use crate::tree::GitTree;

/// The four git object types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectType {
    /// A commit.
    Commit,
    /// A tree.
    Tree,
    /// A blob.
    Blob,
    /// An annotated tag.
    Tag,
}

impl ObjectType {
    /// Every type, in pack type-code order.
    pub const ALL: [ObjectType; 4] = [
        ObjectType::Commit,
        ObjectType::Tree,
        ObjectType::Blob,
        ObjectType::Tag,
    ];

    /// The name in object headers: `commit`, `tree`, `blob`, `tag`.
    pub fn name(self) -> &'static str {
        match self {
            ObjectType::Commit => "commit",
            ObjectType::Tree => "tree",
            ObjectType::Blob => "blob",
            ObjectType::Tag => "tag",
        }
    }

    /// Parses a header name.
    pub fn from_name(name: &[u8]) -> Option<ObjectType> {
        ObjectType::ALL
            .into_iter()
            .find(|t| t.name().as_bytes() == name)
    }

    /// The packfile type code: 1 commit, 2 tree, 3 blob, 4 tag.
    pub fn pack_code(self) -> u8 {
        match self {
            ObjectType::Commit => 1,
            ObjectType::Tree => 2,
            ObjectType::Blob => 3,
            ObjectType::Tag => 4,
        }
    }

    /// Parses a packfile type code (1–4).
    pub fn from_pack_code(code: u8) -> Option<ObjectType> {
        ObjectType::ALL.into_iter().find(|t| t.pack_code() == code)
    }
}

/// The object header `"<type> <len>\0"`.
pub(crate) fn header(kind: ObjectType, len: usize) -> Vec<u8> {
    format!("{} {len}\0", kind.name()).into_bytes()
}

/// An object as type plus body bytes, the form packs and object stores carry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RawObject {
    /// The type.
    pub kind: ObjectType,
    /// The body (without the `"<type> <len>\0"` header).
    pub data: Vec<u8>,
}

impl RawObject {
    /// Wraps a type and body.
    pub fn new(kind: ObjectType, data: Vec<u8>) -> RawObject {
        RawObject { kind, data }
    }

    /// The object's id in `format`.
    pub fn oid(&self, format: ObjectFormat) -> Result<GitOid> {
        format.hash_object(self.kind, &self.data)
    }

    /// Header followed by body: the bytes the id hashes and a loose object inflates to.
    pub fn to_framed(&self) -> Vec<u8> {
        let mut out = header(self.kind, self.data.len());
        out.extend_from_slice(&self.data);
        out
    }

    /// Parses header-plus-body bytes. The header must be canonical: a known type, one space,
    /// a decimal length without leading zeros that matches the body, then NUL.
    pub fn from_framed(bytes: &[u8]) -> Result<RawObject> {
        let nul = bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| malformed("object header", "no NUL"))?;
        let head = &bytes[..nul];
        let space = head
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| malformed("object header", "no space"))?;
        let kind = ObjectType::from_name(&head[..space])
            .ok_or_else(|| malformed("object header", "unknown type"))?;
        let len = parse_decimal(&head[space + 1..])
            .ok_or_else(|| malformed("object header", "bad length"))?;
        let data = &bytes[nul + 1..];
        if data.len() as u64 != len {
            return Err(malformed(
                "object header",
                format!("length {len} but body has {} bytes", data.len()),
            ));
        }
        Ok(RawObject::new(kind, data.to_vec()))
    }

    /// The loose object encoding: zlib over [`RawObject::to_framed`].
    pub fn encode_loose(&self) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        // Writing into a Vec cannot fail.
        let _ = enc.write_all(&self.to_framed());
        enc.finish().unwrap_or_default()
    }

    /// Decodes a loose object, refusing to inflate more than `max_bytes` of body.
    pub fn decode_loose(bytes: &[u8], max_bytes: u64) -> Result<RawObject> {
        let mut out = Vec::new();
        let cap = max_bytes.saturating_add(64);
        ZlibDecoder::new(bytes)
            .take(cap.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| malformed("loose object", format!("zlib: {e}")))?;
        if out.len() as u64 > cap {
            return Err(bee_core::BeeError::too_large(
                "git object bytes",
                max_bytes,
                out.len() as u64,
            ));
        }
        RawObject::from_framed(&out)
    }
}

/// Parses a canonical non-negative decimal (no sign, no leading zeros unless exactly `0`).
pub(crate) fn parse_decimal(text: &[u8]) -> Option<u64> {
    if text.is_empty() || (text.len() > 1 && text[0] == b'0') {
        return None;
    }
    let mut v: u64 = 0;
    for &c in text {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(v)
}

/// A parsed object of any type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitObject {
    /// Blob bytes.
    Blob(Vec<u8>),
    /// A tree.
    Tree(GitTree),
    /// A commit.
    Commit(GitCommit),
    /// An annotated tag.
    Tag(GitTag),
}

impl GitObject {
    /// Parses a body of a known type.
    pub fn parse(kind: ObjectType, data: &[u8], format: ObjectFormat) -> Result<GitObject> {
        Ok(match kind {
            ObjectType::Blob => GitObject::Blob(data.to_vec()),
            ObjectType::Tree => GitObject::Tree(GitTree::parse(data, format)?),
            ObjectType::Commit => GitObject::Commit(GitCommit::parse(data, format)?),
            ObjectType::Tag => GitObject::Tag(GitTag::parse(data, format)?),
        })
    }

    /// The object's type.
    pub fn kind(&self) -> ObjectType {
        match self {
            GitObject::Blob(_) => ObjectType::Blob,
            GitObject::Tree(_) => ObjectType::Tree,
            GitObject::Commit(_) => ObjectType::Commit,
            GitObject::Tag(_) => ObjectType::Tag,
        }
    }

    /// The body bytes; identical to the parsed input.
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            GitObject::Blob(b) => b.clone(),
            GitObject::Tree(t) => t.serialize(),
            GitObject::Commit(c) => c.serialize(),
            GitObject::Tag(t) => t.serialize(),
        }
    }

    /// As a raw object.
    pub fn to_raw(&self) -> RawObject {
        RawObject::new(self.kind(), self.serialize())
    }

    /// The object's id in `format`.
    pub fn oid(&self, format: ObjectFormat) -> Result<GitOid> {
        format.hash_object(self.kind(), &self.serialize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_codes_and_names() {
        for t in ObjectType::ALL {
            assert_eq!(ObjectType::from_name(t.name().as_bytes()), Some(t));
            assert_eq!(ObjectType::from_pack_code(t.pack_code()), Some(t));
        }
        assert_eq!(ObjectType::from_pack_code(5), None);
        assert_eq!(ObjectType::from_name(b"blobs"), None);
    }

    #[test]
    fn loose_round_trip() {
        let obj = RawObject::new(ObjectType::Blob, b"hello\n".to_vec());
        assert_eq!(obj.to_framed(), b"blob 6\0hello\n");
        let loose = obj.encode_loose();
        assert_eq!(RawObject::decode_loose(&loose, 100).unwrap(), obj);
        assert_eq!(
            obj.oid(ObjectFormat::Sha1).unwrap().to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        let big = RawObject::new(ObjectType::Blob, vec![7; 1000]);
        assert!(RawObject::decode_loose(&big.encode_loose(), 10).is_err());
    }

    #[test]
    fn framed_header_rules() {
        assert!(RawObject::from_framed(b"blob 1\0a").is_ok());
        for bad in [
            &b"blob 01\0a"[..],
            b"blob 2\0a",
            b"blob\0",
            b"blob 1a",
            b"blub 1\0a",
            b"blob -1\0a",
            b"blob  1\0a",
        ] {
            assert!(RawObject::from_framed(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_decimal(b"0"), Some(0));
        assert_eq!(parse_decimal(b"99999999999999999999999"), None);
    }
}
