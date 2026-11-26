//! Where git objects come from: a packfile held in memory ([`PackSource`]), a local
//! repository on disk ([`RepoSource`]: its packs, loose objects and, as a fallback, a
//! `git cat-file --batch` subprocess), and the ref lists that go with them.
//!
//! Every object handed out is re-hashed and compared with the id it was asked for, so a
//! corrupt pack or loose file is an `INTEGRITY` error, never a silently different object.
//!
//! Memory: a [`PackSource`] keeps the packed (compressed) bytes and the pack index in memory,
//! plus a bounded cache of recently resolved delta bases ([`PackSource::CACHE_BYTES`]). Objects
//! are inflated on demand, one delta chain at a time.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use bee_core::{BeeError, Reason, Result, Subcode};
use bee_git::pack::DEFAULT_MAX_OBJECT_BYTES;
use bee_git::{
    GitObjectSource, GitOid, ObjectFormat, ObjectType, Pack, PackEntry, PackEntryKind, PackIndex,
    RawObject, delta,
};
use flate2::read::ZlibDecoder;

/// A git ref as the source has it: the full name (`refs/heads/main`), as bytes because git
/// allows names that are not UTF-8, and the object it points at (a commit or a tag).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitRef {
    /// Full ref name.
    pub name: Vec<u8>,
    /// Target object id.
    pub target: GitOid,
}

impl GitRef {
    /// A ref from a UTF-8 name.
    pub fn new(name: impl Into<Vec<u8>>, target: GitOid) -> GitRef {
        GitRef {
            name: name.into(),
            target,
        }
    }
}

/// A shareable, thread-safe object source (thin-pack bases, fallbacks).
pub type SharedSource = Arc<dyn GitObjectSource + Send + Sync>;

pub(crate) fn missing_object(oid: &GitOid) -> BeeError {
    BeeError::new(
        Reason::MissingObject,
        format!("git object {oid} is not in the source"),
    )
    .with_payload("oid", oid.to_hex())
}

fn malformed(what: &str, why: impl std::fmt::Display) -> BeeError {
    BeeError::invalid(Subcode::InvalidField, format!("malformed {what}: {why}"))
}

fn io_error(what: &str, e: std::io::Error) -> BeeError {
    BeeError::new(Reason::Unavailable, format!("{what}: {e}"))
}

/// Checks that `raw` hashes to `oid` (`INTEGRITY` otherwise).
pub(crate) fn verified(oid: &GitOid, raw: RawObject) -> Result<RawObject> {
    let got = raw.oid(oid.format())?;
    if got != *oid {
        return Err(BeeError::new(
            Reason::Integrity,
            format!("git object bytes stored as {oid} hash to {got}"),
        )
        .with_payload("oid", oid.to_hex()));
    }
    Ok(raw)
}

/// Parses a ref list: one `<hex id> <ref name>` per line (the format of `git ls-remote`,
/// `git for-each-ref --format='%(objectname) %(refname)'` and `packed-refs`). Blank lines,
/// `#` comments, peeled lines (`^<id>`) and names ending in `^{}` are skipped. Names are
/// kept as bytes. A tab may separate the two fields instead of a space.
pub fn parse_ref_list(text: &[u8], format: ObjectFormat) -> Result<Vec<GitRef>> {
    let mut out = Vec::new();
    for line in text.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b'#' || line[0] == b'^' {
            continue;
        }
        let n = format.oid_len() * 2;
        if line.len() < n + 2 || !matches!(line[n], b' ' | b'\t') {
            return Err(malformed(
                "ref list",
                format!("line {:?}", String::from_utf8_lossy(line)),
            ));
        }
        let name = &line[n + 1..];
        if name.ends_with(b"^{}") {
            continue;
        }
        out.push(GitRef {
            name: name.to_vec(),
            target: format.parse_hex(&line[..n])?,
        });
    }
    Ok(out)
}

/// Inflates one zlib stream that must produce exactly `size` bytes.
fn inflate(data: &[u8], size: u64) -> Result<Vec<u8>> {
    let cap = usize::try_from(size).map_err(|_| malformed("pack", "object too large"))?;
    let mut out = Vec::with_capacity(cap.min(64 << 20));
    ZlibDecoder::new(data)
        .take(size.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| malformed("pack", format!("zlib: {e}")))?;
    if out.len() as u64 != size {
        return Err(malformed(
            "pack",
            format!("entry inflates to {} bytes, header says {size}", out.len()),
        ));
    }
    Ok(out)
}

/// A FIFO cache of resolved objects keyed by pack offset, bounded in bytes.
#[derive(Debug, Default)]
struct BaseCache {
    objects: BTreeMap<u64, RawObject>,
    order: VecDeque<u64>,
    bytes: usize,
}

impl BaseCache {
    fn get(&self, offset: u64) -> Option<RawObject> {
        self.objects.get(&offset).cloned()
    }

    fn put(&mut self, offset: u64, obj: &RawObject, budget: usize) {
        let len = obj.data.len();
        if len > budget / 8 || self.objects.contains_key(&offset) {
            return;
        }
        while self.bytes + len > budget {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(o) = self.objects.remove(&old) {
                self.bytes -= o.data.len();
            }
        }
        self.bytes += len;
        self.order.push_back(offset);
        self.objects.insert(offset, obj.clone());
    }
}

/// Random access to the objects of one packfile held in memory.
pub struct PackSource {
    format: ObjectFormat,
    bytes: Vec<u8>,
    entries: Vec<PackEntry>,
    index: PackIndex,
    external: Option<SharedSource>,
    cache: Mutex<BaseCache>,
}

impl std::fmt::Debug for PackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackSource")
            .field("format", &self.format)
            .field("bytes", &self.bytes.len())
            .field("objects", &self.entries.len())
            .finish()
    }
}

impl PackSource {
    /// Bytes of resolved objects kept to speed up delta chains.
    pub const CACHE_BYTES: usize = 64 << 20;

    /// Longest delta chain followed before the pack is declared corrupt.
    const MAX_CHAIN: usize = 10_000;

    /// A pack as received from a fetch: parsed and checksum-verified, then indexed by
    /// resolving every object once. Ref-deltas on bases outside the pack (a thin pack) are
    /// resolved through `external` (`MISSING_OBJECT` when absent).
    pub fn from_pack(
        bytes: Vec<u8>,
        format: ObjectFormat,
        external: Option<SharedSource>,
    ) -> Result<PackSource> {
        let ext = external.clone();
        let lookup = move |oid: &GitOid| -> Result<Option<RawObject>> {
            match &ext {
                Some(src) => match src.git_object(oid) {
                    Ok(o) => Ok(Some(o)),
                    Err(e) if e.reason == Reason::MissingObject => Ok(None),
                    Err(e) => Err(e),
                },
                None => Ok(None),
            }
        };
        let (entries, index) = {
            let pack = Pack::parse(&bytes, format)?;
            let index = pack.index(&lookup)?;
            (pack.entries().to_vec(), index)
        };
        Ok(PackSource {
            format,
            bytes,
            entries,
            index,
            external,
            cache: Mutex::new(BaseCache::default()),
        })
    }

    /// A pack with its `.idx` file, as found in a repository's `objects/pack`. The index's
    /// pack checksum must match the pack (`INTEGRITY` otherwise).
    pub fn with_index(bytes: Vec<u8>, idx: &[u8], format: ObjectFormat) -> Result<PackSource> {
        let index = PackIndex::parse(idx, format)?;
        let entries = {
            let pack = Pack::parse(&bytes, format)?;
            if pack.checksum() != index.pack_checksum() {
                return Err(BeeError::new(
                    Reason::Integrity,
                    "pack index does not belong to its pack",
                ));
            }
            pack.entries().to_vec()
        };
        Ok(PackSource {
            format,
            bytes,
            entries,
            index,
            external: None,
            cache: Mutex::new(BaseCache::default()),
        })
    }

    /// The object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Number of objects in the pack.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the pack holds no objects.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when the pack has an object with this id.
    pub fn contains(&self, oid: &GitOid) -> bool {
        self.index.lookup(oid).is_some()
    }

    /// Every object id in the pack, sorted.
    pub fn oids(&self) -> impl Iterator<Item = GitOid> + '_ {
        self.index.entries().iter().map(|e| e.oid)
    }

    fn entry(&self, offset: u64) -> Result<&PackEntry> {
        self.entries
            .binary_search_by_key(&offset, |e| e.offset)
            .map(|i| &self.entries[i])
            .map_err(|_| malformed("pack", format!("no entry at offset {offset}")))
    }

    fn data(&self, e: &PackEntry) -> Result<Vec<u8>> {
        let bytes = self
            .bytes
            .get(e.data.clone())
            .ok_or_else(|| malformed("pack", "entry outside the pack"))?;
        inflate(bytes, e.size)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, BaseCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The object at a pack offset, following its delta chain.
    fn read_at(&self, offset: u64) -> Result<RawObject> {
        let mut chain: Vec<u64> = Vec::new();
        let mut cur = offset;
        let base = loop {
            if let Some(obj) = self.cache().get(cur) {
                break obj;
            }
            if chain.len() > Self::MAX_CHAIN {
                return Err(BeeError::new(
                    Reason::Integrity,
                    format!("pack delta chain at offset {offset} does not end"),
                ));
            }
            let e = self.entry(cur)?;
            match e.kind {
                PackEntryKind::Base(kind) => break RawObject::new(kind, self.data(e)?),
                PackEntryKind::OfsDelta { base_offset } => {
                    chain.push(cur);
                    cur = base_offset;
                }
                PackEntryKind::RefDelta { base } => {
                    chain.push(cur);
                    match self.index.lookup(&base) {
                        Some(o) => cur = o,
                        None => {
                            let ext = self
                                .external
                                .as_ref()
                                .ok_or_else(|| missing_object(&base))?;
                            break verified(&base, ext.git_object(&base)?)?;
                        }
                    }
                }
            }
        };
        let mut obj = base;
        for off in chain.iter().rev() {
            let d = self.data(self.entry(*off)?)?;
            obj = RawObject::new(
                obj.kind,
                delta::apply(&obj.data, &d, DEFAULT_MAX_OBJECT_BYTES)?,
            );
            self.cache().put(*off, &obj, Self::CACHE_BYTES);
        }
        Ok(obj)
    }

    /// The object with this id, or `None` when the pack does not have it.
    pub fn find(&self, oid: &GitOid) -> Result<Option<RawObject>> {
        match self.index.lookup(oid) {
            Some(offset) => Ok(Some(verified(oid, self.read_at(offset)?)?)),
            None => Ok(None),
        }
    }
}

impl GitObjectSource for PackSource {
    fn git_object(&self, oid: &GitOid) -> Result<RawObject> {
        self.find(oid)?.ok_or_else(|| missing_object(oid))
    }
}

/// A `git cat-file --batch` subprocess.
struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    fn spawn(git_dir: &Path) -> Result<CatFile> {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| io_error("starting git cat-file", e))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BeeError::new(Reason::Unavailable, "git cat-file has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BeeError::new(Reason::Unavailable, "git cat-file has no stdout"))?;
        Ok(CatFile {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    fn read(&mut self, oid: &GitOid) -> Result<Option<RawObject>> {
        let io = |e| io_error("git cat-file", e);
        writeln!(self.stdin, "{oid}").map_err(io)?;
        self.stdin.flush().map_err(io)?;
        let mut header = Vec::new();
        self.stdout.read_until(b'\n', &mut header).map_err(io)?;
        let header = header.strip_suffix(b"\n").unwrap_or(&header);
        let fields: Vec<&[u8]> = header.split(|b| *b == b' ').collect();
        match fields.as_slice() {
            [_, b"missing"] | [_, b"ambiguous"] => Ok(None),
            [_, kind, size] => {
                let kind = ObjectType::from_name(kind)
                    .ok_or_else(|| malformed("git cat-file output", "unknown type"))?;
                let size: usize = std::str::from_utf8(size)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| malformed("git cat-file output", "bad size"))?;
                let mut data = vec![0u8; size + 1];
                self.stdout.read_exact(&mut data).map_err(io)?;
                data.pop();
                Ok(Some(RawObject::new(kind, data)))
            }
            _ => Err(malformed(
                "git cat-file output",
                String::from_utf8_lossy(header),
            )),
        }
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A local git repository: its packs and loose objects read directly, with a
/// `git cat-file --batch` fallback for anything else (alternates, promisor remotes).
pub struct RepoSource {
    git_dir: PathBuf,
    format: ObjectFormat,
    packs: Vec<PackSource>,
    catfile: Mutex<Option<CatFile>>,
    use_catfile: bool,
}

impl std::fmt::Debug for RepoSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoSource")
            .field("git_dir", &self.git_dir)
            .field("format", &self.format)
            .field("packs", &self.packs.len())
            .finish()
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| io_error(&format!("reading {}", path.display()), e))
}

#[cfg(unix)]
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

/// Finds the git directory of a repository path: `<path>/.git` (a directory, or a file with
/// a `gitdir:` line), or `<path>` itself when it is a bare repository.
pub fn find_git_dir(path: &Path) -> Result<PathBuf> {
    let dot = path.join(".git");
    if dot.is_dir() {
        return Ok(dot);
    }
    if dot.is_file() {
        let text = read_file(&dot)?;
        let text = String::from_utf8_lossy(&text);
        if let Some(rest) = text.trim().strip_prefix("gitdir:") {
            let p = PathBuf::from(rest.trim());
            return Ok(if p.is_absolute() { p } else { path.join(p) });
        }
    }
    if path.join("objects").is_dir() && path.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    Err(BeeError::new(
        Reason::NotFound,
        format!("{} is not a git repository", path.display()),
    ))
}

/// The object format named in a git `config` file (`extensions.objectFormat`).
fn config_object_format(config: &str) -> Result<ObjectFormat> {
    let mut section = String::new();
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_ascii_lowercase();
            continue;
        }
        if section != "extensions" {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("objectformat")
        {
            let v = v.trim().to_ascii_lowercase();
            return ObjectFormat::from_name(&v).ok_or_else(|| {
                BeeError::invalid(
                    Subcode::InvalidField,
                    format!("unknown git object format {v:?}"),
                )
            });
        }
    }
    Ok(ObjectFormat::Sha1)
}

impl RepoSource {
    /// Opens a repository (working tree or bare): detects the object format and loads every
    /// pack with its index. The cat-file fallback is enabled.
    pub fn open(path: &Path) -> Result<RepoSource> {
        let git_dir = find_git_dir(path)?;
        let config = std::fs::read_to_string(git_dir.join("config")).unwrap_or_default();
        let format = config_object_format(&config)?;
        let mut packs = Vec::new();
        let pack_dir = git_dir.join("objects").join("pack");
        if let Ok(dir) = std::fs::read_dir(&pack_dir) {
            let mut names: Vec<PathBuf> = dir
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "pack"))
                .collect();
            names.sort();
            for pack in names {
                let idx = pack.with_extension("idx");
                if !idx.is_file() {
                    continue;
                }
                packs.push(PackSource::with_index(
                    read_file(&pack)?,
                    &read_file(&idx)?,
                    format,
                )?);
            }
        }
        Ok(RepoSource {
            git_dir,
            format,
            packs,
            catfile: Mutex::new(None),
            use_catfile: true,
        })
    }

    /// Turns the `git cat-file` fallback off (objects must then be in packs or loose).
    pub fn without_catfile(mut self) -> RepoSource {
        self.use_catfile = false;
        self
    }

    /// The git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The repository's object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Number of packs loaded.
    pub fn pack_count(&self) -> usize {
        self.packs.len()
    }

    fn loose(&self, oid: &GitOid) -> Result<Option<RawObject>> {
        let hex = oid.to_hex();
        let path = self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(RawObject::decode_loose(
                &bytes,
                DEFAULT_MAX_OBJECT_BYTES,
            )?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error(&format!("reading {}", path.display()), e)),
        }
    }

    fn catfile(&self, oid: &GitOid) -> Result<Option<RawObject>> {
        if !self.use_catfile {
            return Ok(None);
        }
        let mut guard = self.catfile.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(CatFile::spawn(&self.git_dir)?);
        }
        let Some(cat) = guard.as_mut() else {
            return Ok(None);
        };
        match cat.read(oid) {
            Ok(r) => Ok(r),
            Err(e) => {
                // A broken pipe leaves the process unusable; start afresh next time.
                *guard = None;
                Err(e)
            }
        }
    }

    /// Every ref of the repository: loose refs under `refs/` override `packed-refs`.
    /// Symbolic refs are skipped. Sorted by name.
    pub fn refs(&self) -> Result<Vec<GitRef>> {
        let mut out: BTreeMap<Vec<u8>, GitOid> = BTreeMap::new();
        if let Ok(packed) = std::fs::read(self.git_dir.join("packed-refs")) {
            for r in parse_ref_list(&packed, self.format)? {
                out.insert(r.name, r.target);
            }
        }
        let mut stack = vec![(self.git_dir.join("refs"), b"refs".to_vec())];
        while let Some((dir, prefix)) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let mut name = prefix.clone();
                name.push(b'/');
                name.extend_from_slice(&os_bytes(&e.file_name()));
                let path = e.path();
                if path.is_dir() {
                    stack.push((path, name));
                    continue;
                }
                let text = read_file(&path)?;
                let text = text.strip_suffix(b"\n").unwrap_or(&text);
                if text.starts_with(b"ref:") {
                    continue;
                }
                out.insert(name, self.format.parse_hex(text)?);
            }
        }
        Ok(out
            .into_iter()
            .map(|(name, target)| GitRef { name, target })
            .collect())
    }

    /// The branch `HEAD` names (`refs/heads/main`), when `HEAD` is symbolic.
    pub fn head(&self) -> Option<Vec<u8>> {
        let text = std::fs::read(self.git_dir.join("HEAD")).ok()?;
        let text = text.strip_suffix(b"\n").unwrap_or(&text);
        text.strip_prefix(b"ref: ").map(<[u8]>::to_vec)
    }

    /// The commits a shallow clone lists in `shallow` (their parents are absent).
    pub fn shallow(&self) -> Result<BTreeSet<GitOid>> {
        let Ok(text) = std::fs::read(self.git_dir.join("shallow")) else {
            return Ok(BTreeSet::new());
        };
        text.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| self.format.parse_hex(l))
            .collect()
    }
}

impl GitObjectSource for RepoSource {
    fn git_object(&self, oid: &GitOid) -> Result<RawObject> {
        for p in &self.packs {
            if let Some(o) = p.find(oid)? {
                return Ok(o);
            }
        }
        if let Some(o) = self.loose(oid)? {
            return verified(oid, o);
        }
        match self.catfile(oid)? {
            Some(o) => verified(oid, o),
            None => Err(missing_object(oid)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_git::{DeltaBase, PackObject, write_pack};

    fn blob(data: &[u8]) -> RawObject {
        RawObject::new(ObjectType::Blob, data.to_vec())
    }

    #[test]
    fn pack_source_resolves_delta_chains_and_thin_bases() {
        let f = ObjectFormat::Sha1;
        let a = blob(&b"line\n".repeat(400));
        let mut b_data = a.data.clone();
        b_data.extend_from_slice(b"more\n");
        let b = blob(&b_data);
        let mut c_data = b_data.clone();
        c_data.extend_from_slice(b"and more\n");
        let c = blob(&c_data);
        let (ao, bo, co) = (a.oid(f).unwrap(), b.oid(f).unwrap(), c.oid(f).unwrap());
        let written = write_pack(
            f,
            &[
                PackObject::full(a.clone()),
                PackObject {
                    object: b.clone(),
                    delta_base: Some(DeltaBase::InPack(ao)),
                },
                PackObject {
                    object: c.clone(),
                    delta_base: Some(DeltaBase::InPack(bo)),
                },
            ],
        )
        .unwrap();
        let src = PackSource::from_pack(written.pack.clone(), f, None).unwrap();
        assert_eq!(src.len(), 3);
        // Read in an order that defeats the cache, then again with it warm.
        for _ in 0..2 {
            assert_eq!(src.git_object(&co).unwrap(), c);
            assert_eq!(src.git_object(&ao).unwrap(), a);
            assert_eq!(src.git_object(&bo).unwrap(), b);
        }
        let missing = blob(b"nope").oid(f).unwrap();
        assert_eq!(
            src.git_object(&missing).unwrap_err().reason,
            Reason::MissingObject
        );
        // The same pack through its index file.
        let idx = written.index.write().unwrap();
        let via_idx = PackSource::with_index(written.pack.clone(), &idx, f).unwrap();
        assert_eq!(via_idx.git_object(&co).unwrap(), c);
        assert_eq!(via_idx.oids().count(), 3);

        // A thin pack: c as a ref-delta on b, which only the external source has.
        let thin = write_pack(
            f,
            &[PackObject {
                object: c.clone(),
                delta_base: Some(DeltaBase::External(bo, b.data.clone())),
            }],
        )
        .unwrap();
        assert_eq!(
            PackSource::from_pack(thin.pack.clone(), f, None)
                .unwrap_err()
                .reason,
            Reason::MissingObject
        );
        let base = b.clone();
        let ext: SharedSource = Arc::new(move |o: &GitOid| {
            if *o == bo {
                Ok(base.clone())
            } else {
                Err(missing_object(o))
            }
        });
        let thin_src = PackSource::from_pack(thin.pack, f, Some(ext)).unwrap();
        assert_eq!(thin_src.git_object(&co).unwrap(), c);
        assert!(!thin_src.contains(&bo));
    }

    #[test]
    fn corrupt_pack_bytes_are_rejected() {
        let f = ObjectFormat::Sha1;
        let written = write_pack(f, &[PackObject::full(blob(b"hello"))]).unwrap();
        let mut bad = written.pack.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert_eq!(
            PackSource::from_pack(bad, f, None).unwrap_err().reason,
            Reason::Integrity
        );
        let other = write_pack(f, &[PackObject::full(blob(b"other"))]).unwrap();
        let idx = other.index.write().unwrap();
        assert_eq!(
            PackSource::with_index(written.pack, &idx, f)
                .unwrap_err()
                .reason,
            Reason::Integrity
        );
    }

    #[test]
    fn ref_lists_parse() {
        let f = ObjectFormat::Sha1;
        let a = "1".repeat(40);
        let b = "2".repeat(40);
        let text = format!(
            "# pack-refs with: peeled fully-peeled sorted\n{a} refs/heads/main\n{b}\trefs/tags/v1\n^{a}\n{a} refs/tags/v1^{{}}\n\n"
        );
        let refs = parse_ref_list(text.as_bytes(), f).unwrap();
        assert_eq!(
            refs,
            vec![
                GitRef::new("refs/heads/main", f.parse_hex(a.as_bytes()).unwrap()),
                GitRef::new("refs/tags/v1", f.parse_hex(b.as_bytes()).unwrap()),
            ]
        );
        assert!(parse_ref_list(b"zz refs/heads/x\n", f).is_err());
    }

    #[test]
    fn config_names_the_object_format() {
        assert_eq!(
            config_object_format("[core]\n\tbare = true\n").unwrap(),
            ObjectFormat::Sha1
        );
        assert_eq!(
            config_object_format("[core]\n[extensions]\n\tobjectFormat = sha256\n").unwrap(),
            ObjectFormat::Sha256
        );
        assert!(config_object_format("[extensions]\nobjectformat = md5\n").is_err());
    }
}
