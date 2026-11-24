//! The on-disk [`IndexStore`]: immutable segment files and atomically replaced manifests.
//!
//! Layout under the root directory:
//!
//! ```text
//! orgs/<ORG ULID>/segments/seg-<seq, 16 digits>.seg
//! orgs/<ORG ULID>/manifests/<REPO ULID>/<hex of ref name>.man
//! ```
//!
//! A segment holds a batch of documents and their posting lists:
//!
//! ```text
//! header   "SWSEG001" seq:u64 replaces_through:u64 next_doc:u32
//! contents raw document bytes, back to back
//! postings per trigram, varint deltas of ascending doc numbers
//! footer   ndocs:u32 { doc:u32 blob:34B skip:u8 offset:u64 len:u32 }*
//!          ntri:u32 { trigram:u32 offset:u64 len:u32 count:u32 }*
//! trailer  footer_offset:u64 blake3(header ‖ footer):32B "SWSEGEND"
//! ```
//!
//! Every file is written to a temporary name, synced, renamed into place and the directory
//! synced, so a crash leaves either the old state or the new one. Opening a store reads only
//! headers and footers; posting lists and contents are read on demand with `std::fs` reads
//! (no mmap). Compaction writes one segment with `replaces_through` set to the last segment
//! it replaces; on open, replaced segments are ignored and removed, so a crash during
//! compaction is harmless.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use bee_core::{ObjectId, Result, Ulid, hex};
use bee_store::{OrgId, RepoId};

use super::codec::{
    Dec, Enc, corrupt, decode_manifest, decode_postings, encode_manifest, encode_postings, io_error,
};
use super::{DocNum, Document, IndexStore, Manifest, SkipReason};
use crate::trigram::Trigram;

const SEG_MAGIC: &[u8; 8] = b"SWSEG001";
const SEG_END: &[u8; 8] = b"SWSEGEND";
const HEADER_LEN: usize = 8 + 8 + 8 + 4;
const TRAILER_LEN: usize = 8 + 32 + 8;

#[derive(Clone, Copy)]
struct DocLoc {
    seg: usize,
    blob: ObjectId,
    skip: Option<SkipReason>,
    offset: u64,
    len: u32,
}

struct Segment {
    seq: u64,
    path: PathBuf,
    file: Mutex<File>,
    /// (trigram, offset, len, count), sorted by trigram.
    trigrams: Vec<(u32, u64, u32, u32)>,
}

impl Segment {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut f = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| io_error("seek", &e))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).map_err(|e| io_error("read", &e))?;
        Ok(buf)
    }
}

#[derive(Default)]
struct OrgShard {
    segments: Vec<Segment>,
    by_blob: BTreeMap<ObjectId, DocNum>,
    docs: BTreeMap<DocNum, DocLoc>,
    next_doc: DocNum,
    next_seq: u64,
}

struct ParsedSegment {
    seq: u64,
    replaces_through: u64,
    next_doc: DocNum,
    docs: Vec<(DocNum, ObjectId, Option<SkipReason>, u64, u32)>,
    trigrams: Vec<(u32, u64, u32, u32)>,
}

fn parse_segment(file: &mut File) -> Result<ParsedSegment> {
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| io_error("seek", &e))?;
    if len < (HEADER_LEN + TRAILER_LEN) as u64 {
        return Err(corrupt("segment truncated"));
    }
    let mut read = |off: u64, n: usize| -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(off))
            .map_err(|e| io_error("seek", &e))?;
        let mut b = vec![0u8; n];
        file.read_exact(&mut b).map_err(|e| io_error("read", &e))?;
        Ok(b)
    };
    let trailer = read(len - TRAILER_LEN as u64, TRAILER_LEN)?;
    if &trailer[40..] != SEG_END {
        return Err(corrupt("segment end marker"));
    }
    let footer_offset = u64::from_le_bytes(trailer[..8].try_into().map_err(|_| corrupt("x"))?);
    if footer_offset < HEADER_LEN as u64 || footer_offset > len - TRAILER_LEN as u64 {
        return Err(corrupt("segment footer offset"));
    }
    let header = read(0, HEADER_LEN)?;
    let footer = read(
        footer_offset,
        (len - TRAILER_LEN as u64 - footer_offset) as usize,
    )?;
    let mut hashed = header.clone();
    hashed.extend_from_slice(&footer);
    if bee_object::digest(&hashed) != trailer[8..40] {
        return Err(corrupt("segment digest mismatch"));
    }
    let mut h = Dec::new(&header);
    if h.raw(8)? != SEG_MAGIC {
        return Err(corrupt("segment magic"));
    }
    let seq = h.u64()?;
    let replaces_through = h.u64()?;
    let next_doc = h.u32()?;
    let mut d = Dec::new(&footer);
    let ndocs = d.u32()?;
    let mut docs = Vec::with_capacity(ndocs as usize);
    for _ in 0..ndocs {
        let num = d.u32()?;
        let blob = d.object_id()?;
        let skip = SkipReason::from_code(d.u8()?).map_err(|_| corrupt("skip reason"))?;
        let offset = d.u64()?;
        let dlen = d.u32()?;
        if offset + u64::from(dlen) > footer_offset {
            return Err(corrupt("document range"));
        }
        docs.push((num, blob, skip, offset, dlen));
    }
    let ntri = d.u32()?;
    let mut trigrams = Vec::with_capacity(ntri as usize);
    for _ in 0..ntri {
        let t = d.u32()?;
        let offset = d.u64()?;
        let plen = d.u32()?;
        let count = d.u32()?;
        if offset + u64::from(plen) > footer_offset {
            return Err(corrupt("posting range"));
        }
        trigrams.push((t, offset, plen, count));
    }
    if !d.done() || !trigrams.windows(2).all(|w| w[0].0 < w[1].0) {
        return Err(corrupt("segment footer"));
    }
    Ok(ParsedSegment {
        seq,
        replaces_through,
        next_doc,
        docs,
        trigrams,
    })
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_error("sync directory", &e))
}

/// Writes `bytes` to `path` atomically (temp file, fsync, rename, fsync directory).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| corrupt("path without parent"))?;
    fs::create_dir_all(dir).map_err(|e| io_error("create directory", &e))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| corrupt("file name"))?;
    let tmp = dir.join(format!(".tmp-{name}"));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| io_error("create file", &e))?;
        f.write_all(bytes).map_err(|e| io_error("write", &e))?;
        f.sync_all().map_err(|e| io_error("sync", &e))?;
    }
    fs::rename(&tmp, path).map_err(|e| io_error("rename", &e))?;
    sync_dir(dir)
}

/// Builds a segment file's bytes from documents (with their numbers).
fn build_segment(
    seq: u64,
    replaces_through: u64,
    next_doc: DocNum,
    docs: &[(DocNum, &Document)],
) -> Vec<u8> {
    let mut e = Enc::default();
    e.raw(SEG_MAGIC);
    e.u64(seq);
    e.u64(replaces_through);
    e.u32(next_doc);
    let header = e.buf.clone();
    let mut locs = Vec::with_capacity(docs.len());
    let mut postings: BTreeMap<Trigram, Vec<DocNum>> = BTreeMap::new();
    for (num, d) in docs {
        let offset = e.buf.len() as u64;
        e.raw(&d.content);
        locs.push((*num, d.blob, d.skip, offset, d.content.len() as u32));
        for t in d.trigrams() {
            postings.entry(t).or_default().push(*num);
        }
    }
    let mut tri_dir = Vec::with_capacity(postings.len());
    for (t, list) in &mut postings {
        list.sort_unstable();
        list.dedup();
        let bytes = encode_postings(list);
        tri_dir.push((
            t.0,
            e.buf.len() as u64,
            bytes.len() as u32,
            list.len() as u32,
        ));
        e.raw(&bytes);
    }
    let footer_offset = e.buf.len() as u64;
    let mut f = Enc::default();
    f.u32(locs.len() as u32);
    for (num, blob, skip, offset, len) in &locs {
        f.u32(*num);
        f.raw(blob.as_bytes());
        f.u8(skip.map_or(0, SkipReason::code));
        f.u64(*offset);
        f.u32(*len);
    }
    f.u32(tri_dir.len() as u32);
    for (t, off, len, count) in &tri_dir {
        f.u32(*t);
        f.u64(*off);
        f.u32(*len);
        f.u32(*count);
    }
    let mut hashed = header;
    hashed.extend_from_slice(&f.buf);
    let digest = bee_object::digest(&hashed);
    e.raw(&f.buf);
    e.u64(footer_offset);
    e.raw(&digest);
    e.raw(SEG_END);
    e.buf
}

/// An [`IndexStore`] on the local file system.
pub struct DiskIndexStore {
    root: PathBuf,
    orgs: RwLock<BTreeMap<OrgId, Arc<RwLock<OrgShard>>>>,
    manifests: Mutex<BTreeMap<(OrgId, RepoId, String), Arc<Manifest>>>,
}

impl DiskIndexStore {
    /// Opens (creating if needed) a store rooted at `root`. Orgs load lazily.
    pub fn open(root: impl Into<PathBuf>) -> Result<DiskIndexStore> {
        let root = root.into();
        fs::create_dir_all(root.join("orgs")).map_err(|e| io_error("create root", &e))?;
        Ok(DiskIndexStore {
            root,
            orgs: RwLock::new(BTreeMap::new()),
            manifests: Mutex::new(BTreeMap::new()),
        })
    }

    fn org_dir(&self, org: OrgId) -> PathBuf {
        self.root.join("orgs").join(org.0.to_text())
    }

    fn manifest_path(&self, org: OrgId, repo: RepoId, ref_name: &str) -> PathBuf {
        self.org_dir(org)
            .join("manifests")
            .join(repo.0.to_text())
            .join(format!("{}.man", hex::encode(ref_name.as_bytes())))
    }

    fn load_shard(&self, org: OrgId) -> Result<OrgShard> {
        let dir = self.org_dir(org).join("segments");
        let mut shard = OrgShard::default();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(shard),
            Err(e) => return Err(io_error("list segments", &e)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_error("list segments", &e))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".tmp-") {
                // An interrupted write: never renamed into place, so never visible.
                let _ = fs::remove_file(entry.path());
                continue;
            }
            if name.starts_with("seg-") && name.ends_with(".seg") {
                let path = entry.path();
                let mut file = File::open(&path).map_err(|e| io_error("open segment", &e))?;
                let parsed = parse_segment(&mut file)?;
                found.push((parsed, path, file));
            }
        }
        found.sort_by_key(|(p, _, _)| p.seq);
        let cutoff = found
            .iter()
            .map(|(p, _, _)| p.replaces_through)
            .max()
            .unwrap_or(0);
        for (parsed, path, file) in found {
            shard.next_seq = shard.next_seq.max(parsed.seq + 1);
            shard.next_doc = shard.next_doc.max(parsed.next_doc);
            if parsed.seq <= cutoff {
                let _ = fs::remove_file(&path);
                continue;
            }
            let seg = shard.segments.len();
            for (num, blob, skip, offset, len) in parsed.docs {
                shard.by_blob.insert(blob, num);
                shard.docs.insert(
                    num,
                    DocLoc {
                        seg,
                        blob,
                        skip,
                        offset,
                        len,
                    },
                );
                shard.next_doc = shard.next_doc.max(num + 1);
            }
            shard.segments.push(Segment {
                seq: parsed.seq,
                path,
                file: Mutex::new(file),
                trigrams: parsed.trigrams,
            });
        }
        shard.next_seq = shard.next_seq.max(1);
        Ok(shard)
    }

    fn shard(&self, org: OrgId) -> Result<Arc<RwLock<OrgShard>>> {
        if let Some(s) = self
            .orgs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&org)
        {
            return Ok(s.clone());
        }
        let mut orgs = self.orgs.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(s) = orgs.get(&org) {
            return Ok(s.clone());
        }
        let s = Arc::new(RwLock::new(self.load_shard(org)?));
        orgs.insert(org, s.clone());
        Ok(s)
    }

    fn write_segment(
        &self,
        org: OrgId,
        shard: &mut OrgShard,
        replaces_through: u64,
        docs: &[(DocNum, &Document)],
    ) -> Result<Segment> {
        let seq = shard.next_seq;
        let bytes = build_segment(seq, replaces_through, shard.next_doc, docs);
        let path = self
            .org_dir(org)
            .join("segments")
            .join(format!("seg-{seq:016}.seg"));
        write_atomic(&path, &bytes)?;
        shard.next_seq += 1;
        let mut file = File::open(&path).map_err(|e| io_error("open segment", &e))?;
        let parsed = parse_segment(&mut file)?;
        Ok(Segment {
            seq,
            path,
            file: Mutex::new(file),
            trigrams: parsed.trigrams,
        })
    }

    fn index_segment(shard: &mut OrgShard, seg: Segment, docs: &[(DocNum, &Document)]) {
        let idx = shard.segments.len();
        let mut offset = HEADER_LEN as u64;
        for (num, d) in docs {
            shard.by_blob.insert(d.blob, *num);
            shard.docs.insert(
                *num,
                DocLoc {
                    seg: idx,
                    blob: d.blob,
                    skip: d.skip,
                    offset,
                    len: d.content.len() as u32,
                },
            );
            offset += d.content.len() as u64;
        }
        shard.segments.push(seg);
    }
}

impl IndexStore for DiskIndexStore {
    fn lookup(&self, org: OrgId, blobs: &[ObjectId]) -> Result<Vec<Option<DocNum>>> {
        let shard = self.shard(org)?;
        let s = shard.read().unwrap_or_else(PoisonError::into_inner);
        Ok(blobs.iter().map(|b| s.by_blob.get(b).copied()).collect())
    }

    fn add_documents(&self, org: OrgId, docs: Vec<Document>) -> Result<Vec<DocNum>> {
        let shard = self.shard(org)?;
        let mut s = shard.write().unwrap_or_else(PoisonError::into_inner);
        let mut nums = Vec::with_capacity(docs.len());
        let mut fresh: Vec<(DocNum, &Document)> = Vec::new();
        let mut pending: BTreeMap<ObjectId, DocNum> = BTreeMap::new();
        let mut next = s.next_doc;
        for d in &docs {
            if let Some(n) = s.by_blob.get(&d.blob).or_else(|| pending.get(&d.blob)) {
                nums.push(*n);
                continue;
            }
            pending.insert(d.blob, next);
            fresh.push((next, d));
            nums.push(next);
            next += 1;
        }
        if fresh.is_empty() {
            return Ok(nums);
        }
        let saved = s.next_doc;
        s.next_doc = next;
        match self.write_segment(org, &mut s, 0, &fresh) {
            Ok(seg) => {
                DiskIndexStore::index_segment(&mut s, seg, &fresh);
                Ok(nums)
            }
            Err(e) => {
                s.next_doc = saved;
                Err(e)
            }
        }
    }

    fn document(&self, org: OrgId, doc: DocNum) -> Result<Option<Arc<Document>>> {
        let shard = self.shard(org)?;
        let s = shard.read().unwrap_or_else(PoisonError::into_inner);
        let Some(loc) = s.docs.get(&doc).copied() else {
            return Ok(None);
        };
        let seg = s
            .segments
            .get(loc.seg)
            .ok_or_else(|| corrupt("segment index"))?;
        let content = seg.read_at(loc.offset, loc.len as usize)?;
        Ok(Some(Arc::new(Document {
            blob: loc.blob,
            content,
            skip: loc.skip,
        })))
    }

    fn postings(&self, org: OrgId, trigram: Trigram) -> Result<Vec<DocNum>> {
        let shard = self.shard(org)?;
        let s = shard.read().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<DocNum> = Vec::new();
        for seg in &s.segments {
            if let Ok(i) = seg.trigrams.binary_search_by_key(&trigram.0, |e| e.0) {
                let (_, off, len, count) = seg.trigrams[i];
                let bytes = seg.read_at(off, len as usize)?;
                out.extend(decode_postings(&bytes, count as usize)?);
            }
        }
        if !out.windows(2).all(|w| w[0] < w[1]) {
            out.sort_unstable();
            out.dedup();
        }
        Ok(out)
    }

    fn doc_count(&self, org: OrgId) -> Result<u64> {
        let shard = self.shard(org)?;
        let s = shard.read().unwrap_or_else(PoisonError::into_inner);
        Ok(s.docs.len() as u64)
    }

    fn manifest(&self, org: OrgId, repo: RepoId, ref_name: &str) -> Result<Option<Arc<Manifest>>> {
        let key = (org, repo, ref_name.to_string());
        let mut cache = self
            .manifests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(m) = cache.get(&key) {
            return Ok(Some(m.clone()));
        }
        let bytes = match fs::read(self.manifest_path(org, repo, ref_name)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("read manifest", &e)),
        };
        let m = Arc::new(decode_manifest(&bytes)?);
        if m.org != org || m.repo != repo || m.ref_name != ref_name {
            return Err(corrupt("manifest stored under the wrong name"));
        }
        cache.insert(key, m.clone());
        Ok(Some(m))
    }

    fn put_manifest(&self, manifest: Manifest) -> Result<()> {
        let path = self.manifest_path(manifest.org, manifest.repo, &manifest.ref_name);
        let mut cache = self
            .manifests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        write_atomic(&path, &encode_manifest(&manifest))?;
        cache.insert(
            (manifest.org, manifest.repo, manifest.ref_name.clone()),
            Arc::new(manifest),
        );
        Ok(())
    }

    fn delete_manifest(&self, org: OrgId, repo: RepoId, ref_name: &str) -> Result<bool> {
        let path = self.manifest_path(org, repo, ref_name);
        let mut cache = self
            .manifests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.remove(&(org, repo, ref_name.to_string()));
        match fs::remove_file(&path) {
            Ok(()) => {
                if let Some(dir) = path.parent() {
                    sync_dir(dir)?;
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error("delete manifest", &e)),
        }
    }

    fn list_manifests(&self, org: OrgId) -> Result<Vec<(RepoId, String)>> {
        let dir = self.org_dir(org).join("manifests");
        let repos = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error("list manifests", &e)),
        };
        let mut out = BTreeSet::new();
        for r in repos {
            let r = r.map_err(|e| io_error("list manifests", &e))?;
            let Some(rname) = r.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(ulid) = Ulid::parse(&rname) else {
                continue;
            };
            for f in fs::read_dir(r.path()).map_err(|e| io_error("list manifests", &e))? {
                let f = f.map_err(|e| io_error("list manifests", &e))?;
                let Some(fname) = f.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let Some(h) = fname.strip_suffix(".man") else {
                    continue;
                };
                if fname.starts_with(".tmp-") {
                    continue;
                }
                let Some(bytes) = hex::decode(h) else {
                    continue;
                };
                let Ok(name) = String::from_utf8(bytes) else {
                    continue;
                };
                out.insert((RepoId(ulid), name));
            }
        }
        Ok(out.into_iter().collect())
    }

    fn compact(&self, org: OrgId, keep: &dyn Fn(&ObjectId) -> bool) -> Result<usize> {
        let shard = self.shard(org)?;
        let mut s = shard.write().unwrap_or_else(PoisonError::into_inner);
        let mut kept = Vec::new();
        let mut dropped = 0usize;
        for (num, loc) in &s.docs {
            if keep(&loc.blob) {
                let seg = s
                    .segments
                    .get(loc.seg)
                    .ok_or_else(|| corrupt("segment index"))?;
                let content = seg.read_at(loc.offset, loc.len as usize)?;
                kept.push((
                    *num,
                    Document {
                        blob: loc.blob,
                        content,
                        skip: loc.skip,
                    },
                ));
            } else {
                dropped += 1;
            }
        }
        if dropped == 0 {
            return Ok(0);
        }
        let last = s.segments.iter().map(|g| g.seq).max().unwrap_or(0);
        let refs: Vec<(DocNum, &Document)> = kept.iter().map(|(n, d)| (*n, d)).collect();
        let seg = self.write_segment(org, &mut s, last, &refs)?;
        let old: Vec<PathBuf> = s.segments.iter().map(|g| g.path.clone()).collect();
        s.segments.clear();
        s.by_blob.clear();
        s.docs.clear();
        DiskIndexStore::index_segment(&mut s, seg, &refs);
        for p in old {
            // The new segment already supersedes these; a failed delete is retried on open.
            let _ = fs::remove_file(p);
        }
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_object::blob_id;

    #[test]
    fn contract() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskIndexStore::open(dir.path()).unwrap();
        super::super::contract::run(&store);
    }

    #[test]
    fn reopen_sees_everything() {
        let dir = tempfile::tempdir().unwrap();
        let org = super::super::contract::org();
        let (a, b) = {
            let store = DiskIndexStore::open(dir.path()).unwrap();
            let docs = vec![
                Document::from_bytes(blob_id(b"alpha beta"), b"alpha beta".to_vec()),
                Document::from_bytes(blob_id(b"gamma"), b"gamma".to_vec()),
            ];
            let n = store.add_documents(org, docs).unwrap();
            let m = super::super::contract::manifest(org, 1, n[0], blob_id(b"alpha beta"));
            store.put_manifest(m).unwrap();
            store
                .compact(org, &|x| *x == blob_id(b"alpha beta"))
                .unwrap();
            (n[0], n[1])
        };
        let store = DiskIndexStore::open(dir.path()).unwrap();
        assert_eq!(store.doc_count(org).unwrap(), 1);
        assert_eq!(
            store.lookup(org, &[blob_id(b"alpha beta")]).unwrap(),
            vec![Some(a)]
        );
        let next = store
            .add_documents(
                org,
                vec![Document::from_bytes(blob_id(b"delta"), b"delta".to_vec())],
            )
            .unwrap()[0];
        assert!(next > b, "numbers are not reused after reopening");
        assert_eq!(
            store.postings(org, Trigram::new(b'a', b'l', b'p')).unwrap(),
            vec![a]
        );
        assert_eq!(store.list_manifests(org).unwrap().len(), 1);
        assert!(
            store
                .manifest(org, RepoId(Ulid::from_u128(1)), "heads/main")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn corruption_is_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let org = super::super::contract::org();
        {
            let store = DiskIndexStore::open(dir.path()).unwrap();
            store
                .add_documents(
                    org,
                    vec![Document::from_bytes(blob_id(b"xyz1"), b"xyz1".to_vec())],
                )
                .unwrap();
        }
        let seg_dir = dir
            .path()
            .join("orgs")
            .join(org.0.to_text())
            .join("segments");
        let seg = fs::read_dir(&seg_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut bytes = fs::read(&seg).unwrap();
        let n = bytes.len();
        bytes[n - 50] ^= 0xff;
        fs::write(&seg, &bytes).unwrap();
        let store = DiskIndexStore::open(dir.path()).unwrap();
        let err = store.doc_count(org).unwrap_err();
        assert_eq!(err.reason, bee_core::Reason::Integrity);
        // A leftover temp file from a crashed write is ignored and removed.
        fs::write(seg_dir.join(".tmp-seg-x.seg"), b"junk").unwrap();
        fs::write(&seg, b"").unwrap();
        fs::remove_file(&seg).unwrap();
        let store = DiskIndexStore::open(dir.path()).unwrap();
        assert_eq!(store.doc_count(org).unwrap(), 0);
        assert!(!seg_dir.join(".tmp-seg-x.seg").exists());
    }
}
