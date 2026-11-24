//! Packfiles (version 2, and the identical version 3 layout) and pack index version 2.
//!
//! Pack layout: `PACK`, version (u32 BE), object count (u32 BE), the entries, then the hash
//! of everything before it. An entry header packs the type (1 commit, 2 tree, 3 blob, 4 tag,
//! 6 ofs-delta, 7 ref-delta) and the inflated size; an ofs-delta then names its base by a
//! backwards offset, a ref-delta by id; then a zlib stream. A thin pack has ref-deltas whose
//! bases are not in the pack; the reader asks the caller for them.
//!
//! Index v2 layout: `\xfftOc`, version 2, a 256-entry fan-out table, sorted ids, CRC-32 of
//! each packed entry, 31-bit offsets (high bit = index into a table of 64-bit offsets), the
//! pack checksum and the index checksum.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Range;

use bee_core::{BeeError, Reason, Result};
use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};

use crate::delta;
use crate::error::{integrity, malformed};
use crate::object::{ObjectType, RawObject};
use crate::oid::{GitOid, ObjectFormat};

/// The default cap on one inflated object (the SPEC §4.6 blob hard cap).
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 4 << 30;

/// How an entry is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PackEntryKind {
    /// A whole object.
    Base(ObjectType),
    /// A delta against the entry at this earlier pack offset.
    OfsDelta {
        /// Offset of the base entry.
        base_offset: u64,
    },
    /// A delta against the object with this id (in the pack, or outside it for thin packs).
    RefDelta {
        /// Id of the base object.
        base: GitOid,
    },
}

/// One entry's position and header, as found by [`Pack::parse`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PackEntry {
    /// Offset of the entry header in the pack.
    pub offset: u64,
    /// How the entry is stored.
    pub kind: PackEntryKind,
    /// Inflated size of the stored data (the object, or the delta).
    pub size: u64,
    /// Byte range of the zlib stream.
    pub data: Range<usize>,
    /// CRC-32 of the whole packed entry (header, base reference and zlib stream).
    pub crc32: u32,
}

/// A fully resolved object read from a pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEntry {
    /// Offset of the entry in the pack.
    pub offset: u64,
    /// The object's id.
    pub oid: GitOid,
    /// The object.
    pub object: RawObject,
    /// CRC-32 of the packed entry.
    pub crc32: u32,
    /// Delta chain length (0 for whole objects).
    pub depth: u32,
}

/// A parsed, checksum-verified pack held in memory.
#[derive(Clone, Debug)]
pub struct Pack<'a> {
    bytes: &'a [u8],
    format: ObjectFormat,
    version: u32,
    entries: Vec<PackEntry>,
    by_offset: BTreeMap<u64, usize>,
    checksum: GitOid,
    max_object_bytes: u64,
}

/// Inflates one zlib stream that must produce exactly `expected` bytes. Returns the bytes and
/// how much input the stream used.
fn inflate_exact(input: &[u8], expected: u64) -> Result<(Vec<u8>, usize)> {
    let expected = usize::try_from(expected).map_err(|_| malformed("pack", "object too large"))?;
    let mut d = Decompress::new(true);
    let mut out: Vec<u8> = Vec::with_capacity(expected.min(1 << 20) + 1);
    loop {
        if out.capacity() - out.len() == 0 {
            let want = (expected + 1 - out.len().min(expected)).clamp(1, 1 << 20);
            out.reserve(want);
        }
        let in_before = d.total_in();
        let out_before = d.total_out();
        let consumed = in_before as usize;
        let status = d
            .decompress_vec(&input[consumed..], &mut out, FlushDecompress::None)
            .map_err(|e| malformed("pack", format!("zlib: {e}")))?;
        if out.len() > expected {
            return Err(malformed("pack", "entry inflates past its declared size"));
        }
        match status {
            Status::StreamEnd => break,
            _ if d.total_in() == in_before && d.total_out() == out_before => {
                if consumed >= input.len() {
                    return Err(malformed("pack", "truncated zlib stream"));
                }
                if out.capacity() > out.len() {
                    return Err(malformed("pack", "zlib stream makes no progress"));
                }
            }
            _ => {}
        }
    }
    if out.len() != expected {
        return Err(malformed(
            "pack",
            format!(
                "entry inflates to {} bytes, header says {expected}",
                out.len()
            ),
        ));
    }
    Ok((out, d.total_in() as usize))
}

impl<'a> Pack<'a> {
    /// Parses and verifies a pack: header, every entry header and zlib stream (each must
    /// inflate to its declared size), ofs-delta targets, the object count, and the trailing
    /// checksum (`INTEGRITY` on mismatch). Objects are capped at
    /// [`DEFAULT_MAX_OBJECT_BYTES`].
    pub fn parse(bytes: &'a [u8], format: ObjectFormat) -> Result<Pack<'a>> {
        Pack::parse_with_limit(bytes, format, DEFAULT_MAX_OBJECT_BYTES)
    }

    /// As [`Pack::parse`], with a cap on each object's inflated size (`TOO_LARGE` beyond it).
    pub fn parse_with_limit(
        bytes: &'a [u8],
        format: ObjectFormat,
        max_object_bytes: u64,
    ) -> Result<Pack<'a>> {
        let hlen = format.oid_len();
        if bytes.len() < 12 + hlen {
            return Err(malformed("pack", "shorter than header and checksum"));
        }
        if &bytes[..4] != b"PACK" {
            return Err(malformed("pack", "bad signature"));
        }
        let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != 2 && version != 3 {
            return Err(malformed("pack", format!("unsupported version {version}")));
        }
        let count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let body_end = bytes.len() - hlen;
        let checksum = format.oid_from_bytes(&bytes[body_end..])?;
        if format.hash(&bytes[..body_end])? != checksum {
            return Err(integrity("pack checksum does not match its contents"));
        }
        let mut entries = Vec::new();
        let mut by_offset = BTreeMap::new();
        let mut pos = 12usize;
        for _ in 0..count {
            if pos >= body_end {
                return Err(malformed("pack", "fewer entries than the header counts"));
            }
            let offset = pos;
            let mut c = bytes[pos];
            pos += 1;
            let code = (c >> 4) & 7;
            let mut size = u64::from(c & 0x0f);
            let mut shift = 4u32;
            while c & 0x80 != 0 {
                c = *bytes
                    .get(pos)
                    .filter(|_| pos < body_end)
                    .ok_or_else(|| malformed("pack", "truncated entry header"))?;
                pos += 1;
                if shift > 60 {
                    return Err(malformed("pack", "entry size overflows"));
                }
                size |= u64::from(c & 0x7f) << shift;
                shift += 7;
            }
            if size > max_object_bytes {
                return Err(BeeError::too_large(
                    "git object bytes",
                    max_object_bytes,
                    size,
                ));
            }
            let kind = match code {
                1..=4 => PackEntryKind::Base(
                    ObjectType::from_pack_code(code)
                        .ok_or_else(|| malformed("pack", "bad type"))?,
                ),
                6 => {
                    let mut b = *bytes
                        .get(pos)
                        .filter(|_| pos < body_end)
                        .ok_or_else(|| malformed("pack", "truncated ofs-delta"))?;
                    pos += 1;
                    let mut rel = u64::from(b & 0x7f);
                    while b & 0x80 != 0 {
                        b = *bytes
                            .get(pos)
                            .filter(|_| pos < body_end)
                            .ok_or_else(|| malformed("pack", "truncated ofs-delta"))?;
                        pos += 1;
                        rel = rel
                            .checked_add(1)
                            .and_then(|r| r.checked_mul(128))
                            .map(|r| r | u64::from(b & 0x7f))
                            .ok_or_else(|| malformed("pack", "ofs-delta offset overflows"))?;
                    }
                    let base_offset = (offset as u64)
                        .checked_sub(rel)
                        .filter(|_| rel > 0)
                        .ok_or_else(|| malformed("pack", "ofs-delta points outside the pack"))?;
                    if !by_offset.contains_key(&base_offset) {
                        return Err(malformed("pack", "ofs-delta base is not an entry"));
                    }
                    PackEntryKind::OfsDelta { base_offset }
                }
                7 => {
                    let raw = bytes
                        .get(pos..pos + hlen)
                        .filter(|_| pos + hlen <= body_end)
                        .ok_or_else(|| malformed("pack", "truncated ref-delta"))?;
                    pos += hlen;
                    PackEntryKind::RefDelta {
                        base: format.oid_from_bytes(raw)?,
                    }
                }
                _ => return Err(malformed("pack", format!("reserved entry type {code}"))),
            };
            let (_, used) = inflate_exact(&bytes[pos..body_end], size)?;
            let data = pos..pos + used;
            pos += used;
            let crc32 = crc32fast::hash(&bytes[offset..pos]);
            by_offset.insert(offset as u64, entries.len());
            entries.push(PackEntry {
                offset: offset as u64,
                kind,
                size,
                data,
                crc32,
            });
        }
        if pos != body_end {
            return Err(malformed("pack", "bytes after the last entry"));
        }
        Ok(Pack {
            bytes,
            format,
            version,
            entries,
            by_offset,
            checksum,
            max_object_bytes,
        })
    }

    /// The object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// The pack version (2 or 3).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The trailing checksum.
    pub fn checksum(&self) -> GitOid {
        self.checksum
    }

    /// Every entry, in pack order.
    pub fn entries(&self) -> &[PackEntry] {
        &self.entries
    }

    fn inflate(&self, e: &PackEntry) -> Result<Vec<u8>> {
        inflate_exact(&self.bytes[e.data.clone()], e.size).map(|(d, _)| d)
    }

    /// Resolves every entry and hands each object to `sink`, bases before the deltas built on
    /// them. Ref-deltas whose base is not in the pack (thin packs) get it from `external`;
    /// one it cannot supply is `MISSING_OBJECT`. Only the current delta chain is held in
    /// memory.
    pub fn resolve(
        &self,
        external: &dyn Fn(&GitOid) -> Result<Option<RawObject>>,
        sink: &mut dyn FnMut(ResolvedEntry) -> Result<()>,
    ) -> Result<()> {
        let mut ofs_children: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        let mut ref_children: BTreeMap<GitOid, Vec<usize>> = BTreeMap::new();
        let mut roots = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            match e.kind {
                PackEntryKind::Base(_) => roots.push(i),
                PackEntryKind::OfsDelta { base_offset } => {
                    ofs_children.entry(base_offset).or_default().push(i)
                }
                PackEntryKind::RefDelta { base } => ref_children.entry(base).or_default().push(i),
            }
        }
        let mut done = vec![false; self.entries.len()];
        for i in roots {
            let e = &self.entries[i];
            let PackEntryKind::Base(kind) = e.kind else {
                continue;
            };
            let object = RawObject::new(kind, self.inflate(e)?);
            self.walk(
                i,
                object,
                0,
                &ofs_children,
                &mut ref_children,
                &mut done,
                sink,
            )?;
        }
        // What is left are ref-deltas on bases outside the pack (thin packs).
        let pending: Vec<GitOid> = ref_children.keys().copied().collect();
        for base in pending {
            let Some(kids) = ref_children.remove(&base) else {
                continue;
            };
            let Some(base_obj) = external(&base)? else {
                return Err(BeeError::new(
                    Reason::MissingObject,
                    format!("thin pack delta base {base} is not available"),
                )
                .with_payload("oid", base.to_hex()));
            };
            for k in kids {
                if done[k] {
                    continue;
                }
                let obj = self.apply_entry(k, &base_obj)?;
                self.walk(k, obj, 1, &ofs_children, &mut ref_children, &mut done, sink)?;
            }
        }
        if let Some(i) = done.iter().position(|d| !d) {
            return Err(integrity(format!(
                "pack entry at offset {} has a delta base cycle",
                self.entries[i].offset
            )));
        }
        Ok(())
    }

    fn apply_entry(&self, i: usize, base: &RawObject) -> Result<RawObject> {
        let d = self.inflate(&self.entries[i])?;
        let data = delta::apply(&base.data, &d, self.max_object_bytes)?;
        Ok(RawObject::new(base.kind, data))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        start: usize,
        object: RawObject,
        depth: u32,
        ofs_children: &BTreeMap<u64, Vec<usize>>,
        ref_children: &mut BTreeMap<GitOid, Vec<usize>>,
        done: &mut [bool],
        sink: &mut dyn FnMut(ResolvedEntry) -> Result<()>,
    ) -> Result<()> {
        // Explicit stack: (entry index, object, depth). Children are pushed in reverse so
        // they are emitted in pack order.
        let mut stack = vec![(start, object, depth)];
        while let Some((i, obj, depth)) = stack.pop() {
            let e = &self.entries[i];
            let oid = obj.oid(self.format)?;
            done[i] = true;
            let mut kids: Vec<usize> = ofs_children.get(&e.offset).cloned().unwrap_or_default();
            kids.extend(ref_children.remove(&oid).unwrap_or_default());
            kids.sort_unstable();
            for &k in kids.iter().rev() {
                if !done[k] {
                    let child = self.apply_entry(k, &obj)?;
                    stack.push((k, child, depth + 1));
                }
            }
            sink(ResolvedEntry {
                offset: e.offset,
                oid,
                object: obj,
                crc32: e.crc32,
                depth,
            })?;
        }
        Ok(())
    }

    /// Resolves everything and returns the objects sorted by pack offset.
    pub fn read_all(
        &self,
        external: &dyn Fn(&GitOid) -> Result<Option<RawObject>>,
    ) -> Result<Vec<ResolvedEntry>> {
        let mut out = Vec::with_capacity(self.entries.len());
        self.resolve(external, &mut |r| {
            out.push(r);
            Ok(())
        })?;
        out.sort_by_key(|r| r.offset);
        Ok(out)
    }

    /// Builds the index for this pack (resolving every object to learn its id). Duplicate
    /// ids in one pack are rejected.
    pub fn index(
        &self,
        external: &dyn Fn(&GitOid) -> Result<Option<RawObject>>,
    ) -> Result<PackIndex> {
        let mut entries = Vec::with_capacity(self.entries.len());
        self.resolve(external, &mut |r| {
            entries.push(PackIndexEntry {
                oid: r.oid,
                crc32: r.crc32,
                offset: r.offset,
            });
            Ok(())
        })?;
        PackIndex::new(self.format, entries, self.checksum)
    }

    /// The entry at a pack offset.
    pub fn entry_at(&self, offset: u64) -> Option<&PackEntry> {
        self.by_offset.get(&offset).map(|&i| &self.entries[i])
    }
}

/// One index row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackIndexEntry {
    /// Object id.
    pub oid: GitOid,
    /// CRC-32 of the packed entry.
    pub crc32: u32,
    /// Offset of the entry in the pack.
    pub offset: u64,
}

/// A pack index (version 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackIndex {
    format: ObjectFormat,
    entries: Vec<PackIndexEntry>,
    pack_checksum: GitOid,
}

impl PackIndex {
    /// Builds an index; sorts entries by id and rejects duplicates or ids of another format.
    pub fn new(
        format: ObjectFormat,
        mut entries: Vec<PackIndexEntry>,
        pack_checksum: GitOid,
    ) -> Result<PackIndex> {
        entries.sort_by_key(|e| e.oid);
        for w in entries.windows(2) {
            if w[0].oid == w[1].oid {
                return Err(malformed(
                    "pack index",
                    format!("duplicate object {}", w[0].oid),
                ));
            }
        }
        if entries.iter().any(|e| e.oid.format() != format) || pack_checksum.format() != format {
            return Err(malformed("pack index", "mixed object formats"));
        }
        Ok(PackIndex {
            format,
            entries,
            pack_checksum,
        })
    }

    /// The rows, sorted by id.
    pub fn entries(&self) -> &[PackIndexEntry] {
        &self.entries
    }

    /// The checksum of the pack this index describes.
    pub fn pack_checksum(&self) -> GitOid {
        self.pack_checksum
    }

    /// The pack offset of an object.
    pub fn lookup(&self, oid: &GitOid) -> Option<u64> {
        self.entries
            .binary_search_by(|e| e.oid.cmp(oid))
            .ok()
            .map(|i| self.entries[i].offset)
    }

    /// The index file bytes (version 2), identical to what `git index-pack` writes.
    pub fn write(&self) -> Result<Vec<u8>> {
        let mut out = vec![0xff, b't', b'O', b'c', 0, 0, 0, 2];
        let mut fanout = [0u32; 256];
        for e in &self.entries {
            fanout[usize::from(e.oid.as_bytes()[0])] += 1;
        }
        let mut total = 0u32;
        for f in fanout {
            total += f;
            out.extend_from_slice(&total.to_be_bytes());
        }
        for e in &self.entries {
            out.extend_from_slice(e.oid.as_bytes());
        }
        for e in &self.entries {
            out.extend_from_slice(&e.crc32.to_be_bytes());
        }
        let mut large = Vec::new();
        for e in &self.entries {
            if e.offset < 0x8000_0000 {
                out.extend_from_slice(&(e.offset as u32).to_be_bytes());
            } else {
                out.extend_from_slice(&(0x8000_0000u32 | large.len() as u32).to_be_bytes());
                large.push(e.offset);
            }
        }
        for o in large {
            out.extend_from_slice(&o.to_be_bytes());
        }
        out.extend_from_slice(self.pack_checksum.as_bytes());
        let sum = self.format.hash(&out)?;
        out.extend_from_slice(sum.as_bytes());
        Ok(out)
    }

    /// Parses and verifies index v2 bytes: signature, version, monotonic fan-out matching the
    /// ids, strictly sorted ids, large-offset references, and the index checksum.
    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<PackIndex> {
        let h = format.oid_len();
        let bad = |why: &str| malformed("pack index", why);
        if bytes.len() < 8 + 1024 + 2 * h || bytes[..8] != [0xff, b't', b'O', b'c', 0, 0, 0, 2] {
            return Err(bad("not a version 2 index"));
        }
        let body_end = bytes.len() - h;
        if format.hash(&bytes[..body_end])?.as_bytes() != &bytes[body_end..] {
            return Err(integrity("pack index checksum does not match"));
        }
        let be32 = |at: usize| -> u32 {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let mut prev = 0;
        for i in 0..256 {
            let f = be32(8 + 4 * i);
            if f < prev {
                return Err(bad("fan-out not monotonic"));
            }
            prev = f;
        }
        let n = prev as usize;
        let ids = 8 + 1024;
        let crcs = ids + n * h;
        let offs = crcs + n * 4;
        let large = offs + n * 4;
        let tail = body_end - h;
        if large > tail {
            return Err(bad("truncated tables"));
        }
        let n_large = (tail - large) / 8;
        if large + n_large * 8 != tail {
            return Err(bad("large offset table misaligned"));
        }
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let oid = format.oid_from_bytes(&bytes[ids + i * h..ids + (i + 1) * h])?;
            let raw = be32(offs + 4 * i);
            let offset = if raw & 0x8000_0000 != 0 {
                let j = (raw & 0x7fff_ffff) as usize;
                if j >= n_large {
                    return Err(bad("large offset index out of range"));
                }
                let at = large + 8 * j;
                u64::from_be_bytes(bytes[at..at + 8].try_into().map_err(|_| bad("offset"))?)
            } else {
                u64::from(raw)
            };
            entries.push(PackIndexEntry {
                oid,
                crc32: be32(crcs + 4 * i),
                offset,
            });
        }
        for w in entries.windows(2) {
            if w[0].oid >= w[1].oid {
                return Err(bad("ids not strictly sorted"));
            }
        }
        let mut count = [0u32; 256];
        for e in &entries {
            count[usize::from(e.oid.as_bytes()[0])] += 1;
        }
        let mut total = 0;
        for (i, c) in count.iter().enumerate() {
            total += c;
            if be32(8 + 4 * i) != total {
                return Err(bad("fan-out does not match ids"));
            }
        }
        let pack_checksum = format.oid_from_bytes(&bytes[tail..body_end])?;
        Ok(PackIndex {
            format,
            entries,
            pack_checksum,
        })
    }
}

/// Where a delta's base comes from when writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaBase {
    /// Another object in this pack. Written as an ofs-delta when the base comes earlier,
    /// otherwise as a ref-delta.
    InPack(GitOid),
    /// An object the receiver already has (thin pack): written as a ref-delta, with the
    /// base's bytes supplied here to compute the delta.
    External(GitOid, Vec<u8>),
}

/// An object to write, optionally as a delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackObject {
    /// The object.
    pub object: RawObject,
    /// A suggested delta base. The writer keeps the delta only when it is smaller than the
    /// whole object and the base has the same type.
    pub delta_base: Option<DeltaBase>,
}

impl PackObject {
    /// A whole object.
    pub fn full(object: RawObject) -> PackObject {
        PackObject {
            object,
            delta_base: None,
        }
    }
}

/// A written pack and its index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenPack {
    /// The pack bytes.
    pub pack: Vec<u8>,
    /// Its index.
    pub index: PackIndex,
}

fn entry_header(out: &mut Vec<u8>, code: u8, size: u64) {
    let mut c = (code << 4) | (size & 0x0f) as u8;
    let mut rest = size >> 4;
    while rest != 0 {
        out.push(c | 0x80);
        c = (rest & 0x7f) as u8;
        rest >>= 7;
    }
    out.push(c);
}

fn ofs_encoding(mut rel: u64) -> Vec<u8> {
    let mut buf = vec![(rel & 0x7f) as u8];
    rel >>= 7;
    while rel != 0 {
        rel -= 1;
        buf.push(0x80 | (rel & 0x7f) as u8);
        rel >>= 7;
    }
    buf.reverse();
    buf
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    // Writing into a Vec cannot fail.
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// Writes a version 2 pack of `objects`, in the given order, and its index. Deterministic:
/// the same input gives the same bytes. Duplicate objects are rejected.
pub fn write_pack(format: ObjectFormat, objects: &[PackObject]) -> Result<WrittenPack> {
    let count = u32::try_from(objects.len()).map_err(|_| malformed("pack", "too many objects"))?;
    let mut out = b"PACK".to_vec();
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    let oids = objects
        .iter()
        .map(|o| o.object.oid(format))
        .collect::<Result<Vec<_>>>()?;
    let position: BTreeMap<GitOid, usize> = oids.iter().enumerate().map(|(i, o)| (*o, i)).collect();
    let mut offsets: Vec<u64> = Vec::with_capacity(objects.len());
    let mut index = Vec::with_capacity(objects.len());
    for (i, po) in objects.iter().enumerate() {
        let start = out.len();
        let obj = &po.object;
        let mut written = false;
        if let Some(base) = &po.delta_base {
            let base_info = match base {
                DeltaBase::InPack(oid) => position
                    .get(oid)
                    .filter(|&&j| j != i && objects[j].object.kind == obj.kind)
                    .map(|&j| {
                        (
                            *oid,
                            &objects[j].object.data[..],
                            (j < i).then(|| offsets[j]),
                        )
                    }),
                DeltaBase::External(oid, data) => {
                    let ok = oid.format() == format
                        && format.hash_object(obj.kind, data).ok() == Some(*oid);
                    ok.then_some((*oid, &data[..], None))
                }
            };
            if let Some((base_oid, base_data, base_offset)) = base_info {
                let d = delta::create(base_data, &obj.data);
                if d.len() < obj.data.len() {
                    match base_offset {
                        Some(bo) => {
                            entry_header(&mut out, 6, d.len() as u64);
                            out.extend_from_slice(&ofs_encoding(start as u64 - bo));
                        }
                        None => {
                            entry_header(&mut out, 7, d.len() as u64);
                            out.extend_from_slice(base_oid.as_bytes());
                        }
                    }
                    out.extend_from_slice(&deflate(&d));
                    written = true;
                }
            }
        }
        if !written {
            entry_header(&mut out, obj.kind.pack_code(), obj.data.len() as u64);
            out.extend_from_slice(&deflate(&obj.data));
        }
        offsets.push(start as u64);
        index.push(PackIndexEntry {
            oid: oids[i],
            crc32: crc32fast::hash(&out[start..]),
            offset: start as u64,
        });
    }
    let sum = format.hash(&out)?;
    out.extend_from_slice(sum.as_bytes());
    let index = PackIndex::new(format, index, sum)?;
    Ok(WrittenPack { pack: out, index })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn no_external(_: &GitOid) -> Result<Option<RawObject>> {
        Ok(None)
    }

    fn sample_objects() -> Vec<PackObject> {
        let base: Vec<u8> = (0..4000u32).map(|i| (i % 97) as u8).collect();
        let mut v2 = base.clone();
        v2.extend_from_slice(b"appended");
        let mut v3 = v2.clone();
        v3[10] = 0xff;
        let a = RawObject::new(ObjectType::Blob, base);
        let b = RawObject::new(ObjectType::Blob, v2);
        let c = RawObject::new(ObjectType::Blob, v3);
        let fmt = ObjectFormat::Sha1;
        vec![
            PackObject::full(a.clone()),
            PackObject {
                object: b.clone(),
                delta_base: Some(DeltaBase::InPack(a.oid(fmt).unwrap())),
            },
            PackObject {
                object: c,
                delta_base: Some(DeltaBase::InPack(b.oid(fmt).unwrap())),
            },
            PackObject::full(RawObject::new(ObjectType::Tree, Vec::new())),
        ]
    }

    #[test]
    fn write_read_round_trip() {
        for format in ObjectFormat::ALL {
            let objs: Vec<PackObject> = sample_objects()
                .into_iter()
                .map(|mut o| {
                    if let Some(DeltaBase::InPack(_)) = &o.delta_base {
                        // recompute base ids in this format below
                        o.delta_base = None;
                    }
                    o
                })
                .collect();
            let mut objs = objs;
            let a = objs[0].object.oid(format).unwrap();
            let b = objs[1].object.oid(format).unwrap();
            objs[1].delta_base = Some(DeltaBase::InPack(a));
            objs[2].delta_base = Some(DeltaBase::InPack(b));
            let w = write_pack(format, &objs).unwrap();
            let pack = Pack::parse(&w.pack, format).unwrap();
            assert_eq!(pack.version(), 2);
            assert_eq!(pack.entries().len(), 4);
            assert!(matches!(
                pack.entries()[1].kind,
                PackEntryKind::OfsDelta { .. }
            ));
            assert!(matches!(
                pack.entries()[2].kind,
                PackEntryKind::OfsDelta { .. }
            ));
            let all = pack.read_all(&no_external).unwrap();
            for (r, o) in all.iter().zip(&objs) {
                assert_eq!(r.object, o.object);
            }
            assert_eq!(all[2].depth, 2);
            let idx = pack.index(&no_external).unwrap();
            assert_eq!(idx, w.index);
            let idx_bytes = idx.write().unwrap();
            assert_eq!(PackIndex::parse(&idx_bytes, format).unwrap(), idx);
            assert_eq!(idx.lookup(&all[3].oid), Some(all[3].offset));
            assert_eq!(idx.pack_checksum(), pack.checksum());
            assert!(pack.entry_at(all[1].offset).is_some());
            assert_eq!(pack.format(), format);
        }
    }

    #[test]
    fn thin_packs_need_external_bases() {
        let fmt = ObjectFormat::Sha1;
        let objs = sample_objects();
        let base = objs[0].object.clone();
        let base_oid = base.oid(fmt).unwrap();
        let thin = vec![PackObject {
            object: objs[1].object.clone(),
            delta_base: Some(DeltaBase::External(base_oid, base.data.clone())),
        }];
        let w = write_pack(fmt, &thin).unwrap();
        let pack = Pack::parse(&w.pack, fmt).unwrap();
        assert!(matches!(
            pack.entries()[0].kind,
            PackEntryKind::RefDelta { .. }
        ));
        let err = pack.read_all(&no_external).unwrap_err();
        assert_eq!(err.reason, Reason::MissingObject);
        let got = pack
            .read_all(&|oid: &GitOid| Ok((*oid == base_oid).then(|| base.clone())))
            .unwrap();
        assert_eq!(got[0].object, objs[1].object);
        // A wrong external base is ignored and the object is written whole.
        let bogus = vec![PackObject {
            object: objs[1].object.clone(),
            delta_base: Some(DeltaBase::External(base_oid, b"other".to_vec())),
        }];
        let w = write_pack(fmt, &bogus).unwrap();
        let pack = Pack::parse(&w.pack, fmt).unwrap();
        assert!(matches!(
            pack.entries()[0].kind,
            PackEntryKind::Base(ObjectType::Blob)
        ));
    }

    #[test]
    fn in_pack_ref_delta_to_later_base() {
        let fmt = ObjectFormat::Sha1;
        let objs = sample_objects();
        let a = objs[0].object.clone();
        let later = vec![
            PackObject {
                object: objs[1].object.clone(),
                delta_base: Some(DeltaBase::InPack(a.oid(fmt).unwrap())),
            },
            PackObject::full(a),
        ];
        let w = write_pack(fmt, &later).unwrap();
        let pack = Pack::parse(&w.pack, fmt).unwrap();
        assert!(matches!(
            pack.entries()[0].kind,
            PackEntryKind::RefDelta { .. }
        ));
        let all = pack.read_all(&no_external).unwrap();
        assert_eq!(all[0].object, objs[1].object);
    }

    #[test]
    fn corruption_is_detected() {
        let fmt = ObjectFormat::Sha1;
        let w = write_pack(fmt, &sample_objects()).unwrap();
        let mut bad = w.pack.clone();
        bad[20] ^= 1;
        assert_eq!(
            Pack::parse(&bad, fmt).unwrap_err().reason,
            Reason::Integrity
        );
        assert!(Pack::parse(&w.pack[..10], fmt).is_err());
        assert!(Pack::parse(&w.pack, ObjectFormat::Sha256).is_err());
        let mut idx = w.index.write().unwrap();
        idx[100] ^= 1;
        assert!(PackIndex::parse(&idx, fmt).is_err());
        assert!(Pack::parse_with_limit(&w.pack, fmt, 10).unwrap_err().reason == Reason::TooLarge);
        // Duplicate objects are rejected by the index.
        let dup = vec![sample_objects()[3].clone(), sample_objects()[3].clone()];
        assert!(write_pack(fmt, &dup).is_err());
    }

    #[test]
    fn large_offsets_in_index() {
        let fmt = ObjectFormat::Sha1;
        let entries = vec![
            PackIndexEntry {
                oid: GitOid::Sha1([1; 20]),
                crc32: 7,
                offset: 12,
            },
            PackIndexEntry {
                oid: GitOid::Sha1([2; 20]),
                crc32: 8,
                offset: 0x1_0000_0000,
            },
        ];
        let idx = PackIndex::new(fmt, entries, GitOid::Sha1([9; 20])).unwrap();
        let bytes = idx.write().unwrap();
        assert_eq!(PackIndex::parse(&bytes, fmt).unwrap(), idx);
    }

    #[test]
    fn ofs_encoding_round_trips() {
        for rel in [1u64, 127, 128, 129, 16_511, 16_512, 1 << 40] {
            let enc = ofs_encoding(rel);
            let mut v = u64::from(enc[0] & 0x7f);
            for &b in &enc[1..] {
                v = ((v + 1) << 7) | u64::from(b & 0x7f);
            }
            assert_eq!(v, rel);
        }
    }

    proptest! {
        #[test]
        fn parse_never_panics(data in proptest::collection::vec(any::<u8>(), 0..300)) {
            let mut bytes = b"PACK\0\0\0\x02\0\0\0\x03".to_vec();
            bytes.extend_from_slice(&data);
            let sum = ObjectFormat::Sha1.hash(&bytes).unwrap();
            bytes.extend_from_slice(sum.as_bytes());
            if let Ok(p) = Pack::parse(&bytes, ObjectFormat::Sha1) {
                let _ = p.read_all(&no_external);
            }
            let _ = PackIndex::parse(&data, ObjectFormat::Sha1);
        }
    }
}
