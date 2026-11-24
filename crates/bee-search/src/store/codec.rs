//! Binary encoding helpers for index files, and the manifest file format.
//!
//! All integers are little-endian; posting lists use LEB128 varints of ascending deltas.
//! Manifest files end with the BLAKE3 digest of everything before it, checked on read.

use std::collections::BTreeMap;

use bee_core::{BeeError, Mode, OBJECT_ID_LEN, ObjectId, Reason, RepoPath, Result, Ulid};
use bee_store::{OrgId, RepoId};

use super::{DocNum, FileEntry, Manifest};
use crate::lang::Language;

/// The error for a corrupt or truncated index file.
pub fn corrupt(what: &str) -> BeeError {
    BeeError::new(Reason::Integrity, format!("corrupt index file: {what}"))
}

/// The error for an I/O failure.
pub fn io_error(what: &str, e: &std::io::Error) -> BeeError {
    BeeError::new(Reason::Unavailable, format!("index storage {what}: {e}"))
}

/// An append-only byte encoder.
#[derive(Default)]
pub struct Enc {
    /// The bytes so far.
    pub buf: Vec<u8>,
}

impl Enc {
    /// One byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    /// Two bytes.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Four bytes.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Eight bytes.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Raw bytes.
    pub fn raw(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    /// Length-prefixed bytes.
    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.raw(v);
    }
    /// LEB128 varint.
    pub fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
}

/// A cursor over encoded bytes; every read fails with `INTEGRITY` when out of data.
pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    /// A decoder at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Dec<'a> {
        Dec { buf, pos: 0 }
    }
    /// True when every byte was consumed.
    pub fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
    /// Raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| corrupt("length overflow"))?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| corrupt("truncated"))?;
        self.pos = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.raw(N)?);
        Ok(a)
    }
    /// One byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.arr::<1>()?[0])
    }
    /// Two bytes.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    /// Four bytes.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    /// Eight bytes.
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    /// Length-prefixed bytes.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.raw(n)
    }
    /// LEB128 varint.
    pub fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(corrupt("varint too long"))
    }
    /// An object id.
    pub fn object_id(&mut self) -> Result<ObjectId> {
        ObjectId::from_bytes(self.raw(OBJECT_ID_LEN)?).map_err(|_| corrupt("bad object id"))
    }
    /// A ULID.
    pub fn ulid(&mut self) -> Result<Ulid> {
        Ok(Ulid::from_bytes(self.arr()?))
    }
}

/// Encodes a sorted posting list as varint deltas.
pub fn encode_postings(docs: &[DocNum]) -> Vec<u8> {
    let mut e = Enc::default();
    let mut prev = 0u64;
    for (i, d) in docs.iter().enumerate() {
        let d = u64::from(*d);
        e.varint(if i == 0 { d } else { d - prev });
        prev = d;
    }
    e.buf
}

/// Decodes `count` postings.
pub fn decode_postings(bytes: &[u8], count: usize) -> Result<Vec<DocNum>> {
    let mut d = Dec::new(bytes);
    let mut out = Vec::with_capacity(count);
    let mut prev = 0u64;
    for i in 0..count {
        let v = d.varint()?;
        let cur = if i == 0 { v } else { prev + v };
        out.push(DocNum::try_from(cur).map_err(|_| corrupt("doc number overflow"))?);
        prev = cur;
    }
    Ok(out)
}

const MANIFEST_MAGIC: &[u8; 8] = b"SWMAN001";
const NO_DOC: u32 = u32::MAX;

/// Encodes a manifest file.
pub fn encode_manifest(m: &Manifest) -> Vec<u8> {
    let mut e = Enc::default();
    e.raw(MANIFEST_MAGIC);
    e.raw(&m.org.0.to_bytes());
    e.raw(&m.repo.0.to_bytes());
    e.bytes(m.ref_name.as_bytes());
    e.raw(m.changeset.as_bytes());
    e.u32(m.files.len() as u32);
    for (path, f) in &m.files {
        e.bytes(path.as_bytes());
        e.raw(f.blob.as_bytes());
        e.u8(f.mode.as_u64() as u8);
        e.u64(f.size);
        e.u32(f.doc.unwrap_or(NO_DOC));
        e.u16(f.language.map_or(0, Language::code));
    }
    let digest = bee_object::digest(&e.buf);
    e.raw(&digest);
    e.buf
}

/// Decodes and verifies a manifest file.
pub fn decode_manifest(bytes: &[u8]) -> Result<Manifest> {
    if bytes.len() < 32 {
        return Err(corrupt("manifest truncated"));
    }
    let (body, digest) = bytes.split_at(bytes.len() - 32);
    if bee_object::digest(body) != digest {
        return Err(corrupt("manifest digest mismatch"));
    }
    let mut d = Dec::new(body);
    if d.raw(8)? != MANIFEST_MAGIC {
        return Err(corrupt("manifest magic"));
    }
    let org = OrgId(d.ulid()?);
    let repo = RepoId(d.ulid()?);
    let ref_name = String::from_utf8(d.bytes()?.to_vec()).map_err(|_| corrupt("ref name"))?;
    let changeset = d.object_id()?;
    let n = d.u32()?;
    let mut files = BTreeMap::new();
    for _ in 0..n {
        let path = RepoPath::new(d.bytes()?.to_vec()).map_err(|_| corrupt("path"))?;
        let blob = d.object_id()?;
        let mode = Mode::from_u64(u64::from(d.u8()?)).map_err(|_| corrupt("mode"))?;
        let size = d.u64()?;
        let doc = match d.u32()? {
            NO_DOC => None,
            n => Some(n),
        };
        let language = match d.u16()? {
            0 => None,
            c => Some(Language::from_code(c).ok_or_else(|| corrupt("language"))?),
        };
        files.insert(
            path,
            FileEntry {
                blob,
                mode,
                size,
                doc,
                language,
            },
        );
    }
    if !d.done() {
        return Err(corrupt("trailing manifest bytes"));
    }
    Ok(Manifest {
        org,
        repo,
        ref_name,
        changeset,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postings_round_trip() {
        let v = vec![0, 1, 5, 300, 70_000, u32::MAX - 1];
        assert_eq!(decode_postings(&encode_postings(&v), v.len()).unwrap(), v);
        assert!(decode_postings(&[0x80], 1).is_err());
    }

    #[test]
    fn manifest_round_trip_and_tamper() {
        let m = super::super::contract::manifest(
            super::super::contract::org(),
            3,
            9,
            bee_object::blob_id(b"x"),
        );
        let bytes = encode_manifest(&m);
        assert_eq!(decode_manifest(&bytes).unwrap(), m);
        let mut bad = bytes.clone();
        bad[20] ^= 1;
        assert_eq!(decode_manifest(&bad).unwrap_err().reason, Reason::Integrity);
        assert!(decode_manifest(&bytes[..10]).is_err());
    }

    #[test]
    fn dec_errors() {
        let mut d = Dec::new(&[1, 2]);
        assert!(d.u32().is_err());
        let mut e = Enc::default();
        e.u16(7);
        e.bytes(b"ab");
        e.u64(9);
        let mut d = Dec::new(&e.buf);
        assert_eq!(d.u16().unwrap(), 7);
        assert_eq!(d.bytes().unwrap(), b"ab");
        assert_eq!(d.u64().unwrap(), 9);
        assert!(d.done());
    }
}
