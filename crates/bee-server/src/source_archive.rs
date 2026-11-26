//! Source archives of a tree (`docs/spec/forge-community.md` §2, GitHub's tarball and
//! zipball): `tar.gz` and `zip` writers that emit their output entry by entry, so the archive
//! server streams it while it walks the tree.
//!
//! The bytes are a pure function of the tree, the prefix and the format, which is what makes an
//! archive cacheable by tree id: entries come in path order, every timestamp is 1980-01-01
//! 00:00:00 UTC (the earliest a zip can hold), owners are 0, and the gzip header carries no
//! name or time. Files are mode 0644, executables 0755, symlinks hold their target, and
//! directories are listed before their contents. Submodule pointers are left out, as git
//! archive does.
//!
//! - `tar.gz`: POSIX ustar; a path or link target over 100 bytes is carried in a PAX
//!   extended header (`path=`, `linkpath=`).
//! - `zip`: deflate (stored for empty entries), UTF-8 names, Unix modes in the external
//!   attributes. Without zip64, an archive is limited to 65,535 entries and 4 GiB.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};

/// Every timestamp in an archive: 1980-01-01T00:00:00Z.
pub const MTIME: u64 = 315_532_800;

/// Zip limits without zip64.
const ZIP_MAX_ENTRIES: usize = u16::MAX as usize;
const ZIP_MAX_BYTES: u64 = u32::MAX as u64;

/// An archive format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// gzip-compressed tar.
    TarGz,
    /// zip.
    Zip,
}

impl Format {
    /// The file extension.
    pub fn ext(self) -> &'static str {
        match self {
            Format::TarGz => "tar.gz",
            Format::Zip => "zip",
        }
    }

    /// The media type.
    pub fn content_type(self) -> &'static str {
        match self {
            Format::TarGz => "application/gzip",
            Format::Zip => "application/zip",
        }
    }
}

/// What an archive entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// An executable file.
    Executable,
    /// A symbolic link; the data is its target.
    Symlink,
    /// A directory (no data).
    Dir,
}

impl Kind {
    fn unix_mode(self) -> u32 {
        match self {
            Kind::File => 0o100644,
            Kind::Executable => 0o100755,
            Kind::Symlink => 0o120777,
            Kind::Dir => 0o040755,
        }
    }
}

/// The parts of an archive storage key, `archives/<repo>/<tree hex>/<prefix>.<ext>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveKey {
    /// The repo id.
    pub repo: String,
    /// The tree id, hex of its raw bytes.
    pub tree_hex: String,
    /// The top-level directory name.
    pub prefix: String,
    /// The format.
    pub format: Format,
}

impl ArchiveKey {
    /// Parses a storage key; `None` when it is not one.
    pub fn parse(key: &str) -> Option<ArchiveKey> {
        let rest = key.strip_prefix("archives/")?;
        let mut parts = rest.splitn(3, '/');
        let repo = parts.next()?.to_string();
        let tree_hex = parts.next()?.to_string();
        let file = parts.next()?;
        let (prefix, format) = if let Some(p) = file.strip_suffix(".tar.gz") {
            (p, Format::TarGz)
        } else {
            (file.strip_suffix(".zip")?, Format::Zip)
        };
        let ok = |s: &str| !s.is_empty() && !s.contains('/') && !s.contains("..");
        (ok(&repo)
            && ok(prefix)
            && !tree_hex.is_empty()
            && tree_hex.len() % 2 == 0
            && tree_hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| ArchiveKey {
            repo,
            tree_hex,
            prefix: prefix.to_string(),
            format,
        })
    }

    /// The raw bytes of the tree id.
    pub fn tree_raw(&self) -> Vec<u8> {
        (0..self.tree_hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(&self.tree_hex[i..i + 2], 16).ok())
            .collect()
    }

    /// The download file name.
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.prefix, self.format.ext())
    }
}

/// An archive being written. [`Writer::add`] appends an entry, [`Writer::take`] hands out the
/// bytes produced so far, and [`Writer::finish`] returns the rest.
pub enum Writer {
    /// tar.gz.
    TarGz(GzEncoder<Vec<u8>>),
    /// zip.
    Zip(ZipState),
}

/// The state of a zip being written.
pub struct ZipState {
    out: Vec<u8>,
    offset: u64,
    central: Vec<u8>,
    entries: usize,
}

/// Why an archive cannot be written.
#[derive(Debug)]
pub enum ArchiveError {
    /// Over a format limit.
    TooLarge(String),
    /// Compression failed.
    Io(std::io::Error),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::TooLarge(m) => f.write_str(m),
            ArchiveError::Io(e) => write!(f, "compression failed: {e}"),
        }
    }
}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

/// Checks the zip limits for `entries` entries of `bytes` content bytes before streaming
/// starts (deflate output can exceed its input by a few bytes per 32 KiB, plus headers).
pub fn check_limits(format: Format, entries: usize, bytes: u64) -> Result<(), ArchiveError> {
    if format == Format::Zip {
        let overhead = entries as u64 * 200 + bytes / 1024;
        if entries > ZIP_MAX_ENTRIES || bytes.saturating_add(overhead) > ZIP_MAX_BYTES {
            return Err(ArchiveError::TooLarge(format!(
                "a zip archive holds at most {ZIP_MAX_ENTRIES} entries and 4 GiB; use tar.gz"
            )));
        }
    }
    Ok(())
}

impl Writer {
    /// A new archive.
    pub fn new(format: Format) -> Writer {
        match format {
            Format::TarGz => Writer::TarGz(
                flate2::GzBuilder::new()
                    .mtime(0)
                    .write(Vec::new(), Compression::default()),
            ),
            Format::Zip => Writer::Zip(ZipState {
                out: Vec::new(),
                offset: 0,
                central: Vec::new(),
                entries: 0,
            }),
        }
    }

    /// Appends an entry at `path` (without a trailing slash; directories get one).
    pub fn add(&mut self, path: &str, kind: Kind, data: &[u8]) -> Result<(), ArchiveError> {
        match self {
            Writer::TarGz(enc) => {
                enc.write_all(&tar_entry(path, kind, data))?;
                Ok(())
            }
            Writer::Zip(z) => z.add(path, kind, data),
        }
    }

    /// The bytes produced so far, removed from the writer.
    pub fn take(&mut self) -> Vec<u8> {
        match self {
            Writer::TarGz(enc) => std::mem::take(enc.get_mut()),
            Writer::Zip(z) => std::mem::take(&mut z.out),
        }
    }

    /// Ends the archive; returns the remaining bytes.
    pub fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        match self {
            Writer::TarGz(mut enc) => {
                enc.write_all(&[0u8; 1024])?;
                Ok(enc.finish()?)
            }
            Writer::Zip(z) => z.finish(),
        }
    }
}

// ----- tar ---------------------------------------------------------------------------------

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    let start = text.len().saturating_sub(digits);
    field[..digits].copy_from_slice(&text.as_bytes()[start..]);
    field[digits] = 0;
}

fn ustar_header(name: &[u8], mode: u32, size: u64, typeflag: u8, link: &[u8]) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    octal(&mut h[100..108], u64::from(mode & 0o7777));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], MTIME);
    h[156] = typeflag;
    let l = link.len().min(100);
    h[157..157 + l].copy_from_slice(&link[..l]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    let chk = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(chk.as_bytes());
    h
}

/// One PAX record, `<len> <key>=<value>\n`, where `<len>` counts the whole record.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3; // space, '=', newline
    let mut len = body + 1;
    while len.to_string().len() + body > len {
        len += 1;
    }
    let mut rec = format!("{len} {key}=").into_bytes();
    rec.extend_from_slice(value);
    rec.push(b'\n');
    rec
}

fn padded(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(data);
    let pad = data.len().div_ceil(512) * 512 - data.len();
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// The tar bytes of one entry (with a PAX header first when a name does not fit).
fn tar_entry(path: &str, kind: Kind, data: &[u8]) -> Vec<u8> {
    let mut name = path.as_bytes().to_vec();
    if kind == Kind::Dir {
        name.push(b'/');
    }
    let link: &[u8] = if kind == Kind::Symlink { data } else { &[] };
    let mut out = Vec::new();
    let mut pax = Vec::new();
    if name.len() > 100 {
        pax.extend(pax_record("path", &name));
    }
    if link.len() > 100 {
        pax.extend(pax_record("linkpath", link));
    }
    if !pax.is_empty() {
        out.extend_from_slice(&ustar_header(
            b"././@PaxHeader",
            0o644,
            pax.len() as u64,
            b'x',
            &[],
        ));
        padded(&mut out, &pax);
    }
    let (typeflag, size) = match kind {
        Kind::File | Kind::Executable => (b'0', data.len() as u64),
        Kind::Symlink => (b'2', 0),
        Kind::Dir => (b'5', 0),
    };
    out.extend_from_slice(&ustar_header(&name, kind.unix_mode(), size, typeflag, link));
    if size > 0 {
        padded(&mut out, data);
    }
    out
}

// ----- zip ---------------------------------------------------------------------------------

/// MS-DOS date of 1980-01-01 (time 00:00:00 is 0).
const DOS_DATE: u16 = (1 << 5) | 1;

impl ZipState {
    fn add(&mut self, path: &str, kind: Kind, data: &[u8]) -> Result<(), ArchiveError> {
        if self.entries >= ZIP_MAX_ENTRIES {
            return Err(ArchiveError::TooLarge(format!(
                "a zip archive holds at most {ZIP_MAX_ENTRIES} entries"
            )));
        }
        let mut name = path.as_bytes().to_vec();
        if kind == Kind::Dir {
            name.push(b'/');
        }
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let (method, body): (u16, Vec<u8>) = if data.is_empty() {
            (0, Vec::new())
        } else {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(data)?;
            (8, enc.finish()?)
        };
        let too_big = |n: u64| n > ZIP_MAX_BYTES;
        if too_big(data.len() as u64) || too_big(body.len() as u64) || too_big(self.offset) {
            return Err(ArchiveError::TooLarge(
                "a zip archive is at most 4 GiB; use tar.gz".into(),
            ));
        }
        let flags: u16 = 0x0800; // UTF-8 names
        let mut local = Vec::with_capacity(30 + name.len());
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&flags.to_le_bytes());
        local.extend_from_slice(&method.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&DOS_DATE.to_le_bytes());
        local.extend_from_slice(&crc.sum().to_le_bytes());
        local.extend_from_slice(&(body.len() as u32).to_le_bytes());
        local.extend_from_slice(&(data.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&name);
        let c = &mut self.central;
        c.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        c.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // made by Unix, 2.0
        c.extend_from_slice(&20u16.to_le_bytes());
        c.extend_from_slice(&flags.to_le_bytes());
        c.extend_from_slice(&method.to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes());
        c.extend_from_slice(&DOS_DATE.to_le_bytes());
        c.extend_from_slice(&crc.sum().to_le_bytes());
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(&(data.len() as u32).to_le_bytes());
        c.extend_from_slice(&(name.len() as u16).to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes()); // extra
        c.extend_from_slice(&0u16.to_le_bytes()); // comment
        c.extend_from_slice(&0u16.to_le_bytes()); // disk
        c.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        let dos_dir: u32 = if kind == Kind::Dir { 0x10 } else { 0 };
        c.extend_from_slice(&((kind.unix_mode() << 16) | dos_dir).to_le_bytes());
        c.extend_from_slice(&(self.offset as u32).to_le_bytes());
        c.extend_from_slice(&name);
        self.offset += (local.len() + body.len()) as u64;
        self.out.extend_from_slice(&local);
        self.out.extend_from_slice(&body);
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, ArchiveError> {
        let cd_size = self.central.len() as u64;
        if self.offset + cd_size > ZIP_MAX_BYTES {
            return Err(ArchiveError::TooLarge(
                "a zip archive is at most 4 GiB; use tar.gz".into(),
            ));
        }
        let mut out = std::mem::take(&mut self.out);
        out.extend_from_slice(&self.central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(self.entries as u16).to_le_bytes());
        out.extend_from_slice(&(self.entries as u16).to_le_bytes());
        out.extend_from_slice(&(cd_size as u32).to_le_bytes());
        out.extend_from_slice(&(self.offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        Ok(out)
    }
}

/// The directories to list before `path` that `seen` does not hold yet (each once).
pub fn new_parents(path: &str, seen: &mut std::collections::BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = path[at..].find('/') {
        let dir = &path[..at + i];
        if seen.insert(dir.to_string()) {
            out.push(dir.to_string());
        }
        at += i + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn sample(format: Format) -> Vec<u8> {
        let long = format!("p/{}/deep.txt", "d".repeat(120));
        let mut w = Writer::new(format);
        let mut out = Vec::new();
        w.add("p", Kind::Dir, &[]).unwrap();
        w.add("p/a.txt", Kind::File, b"alpha\n").unwrap();
        out.extend(w.take());
        w.add("p/run.sh", Kind::Executable, b"#!/bin/sh\n").unwrap();
        w.add("p/link", Kind::Symlink, b"a.txt").unwrap();
        w.add("p/empty", Kind::File, b"").unwrap();
        w.add(&long, Kind::File, b"deep\n").unwrap();
        out.extend(w.take());
        out.extend(w.finish().unwrap());
        out
    }

    #[test]
    fn keys_parse_and_reject_traversal() {
        let k = ArchiveKey::parse("archives/R1/abcd/app-1.tar.gz").unwrap();
        assert_eq!(
            (
                k.repo.as_str(),
                k.tree_raw(),
                k.format,
                k.file_name().as_str()
            ),
            ("R1", vec![0xab, 0xcd], Format::TarGz, "app-1.tar.gz")
        );
        assert_eq!(
            ArchiveKey::parse("archives/R1/ab/x.zip").unwrap().format,
            Format::Zip
        );
        for bad in [
            "archives/R1/abc/x.zip",
            "archives/R1/zz/x.zip",
            "archives/R1/ab/x.rar",
            "archives/R1/ab/../x.zip",
            "releases/R1/ab/x.zip",
            "archives/R1/ab/.zip",
        ] {
            assert!(ArchiveKey::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tar_gz_round_trips_through_a_reader_and_is_deterministic() {
        let a = sample(Format::TarGz);
        assert_eq!(a, sample(Format::TarGz), "a pure function of the input");
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(&a[..])
            .read_to_end(&mut tar)
            .unwrap();
        // Walk the headers: names, types and sizes.
        let mut i = 0;
        let mut seen = Vec::new();
        let mut pax_path: Option<String> = None;
        while i + 512 <= tar.len() && tar[i] != 0 {
            let h = &tar[i..i + 512];
            let name = String::from_utf8_lossy(&h[..100])
                .trim_end_matches('\0')
                .to_string();
            let size = u64::from_str_radix(
                String::from_utf8_lossy(&h[124..135]).trim_end_matches('\0'),
                8,
            )
            .unwrap() as usize;
            let mtime = u64::from_str_radix(
                String::from_utf8_lossy(&h[136..147]).trim_end_matches('\0'),
                8,
            )
            .unwrap();
            let data = &tar[i + 512..i + 512 + size];
            if h[156] == b'x' {
                let text = String::from_utf8_lossy(data);
                let (_, kv) = text.split_once(' ').unwrap();
                pax_path = Some(kv.trim_end().strip_prefix("path=").unwrap().to_string());
            } else {
                assert_eq!(mtime, MTIME);
                let name = pax_path.take().unwrap_or(name);
                let link = String::from_utf8_lossy(&h[157..257])
                    .trim_end_matches('\0')
                    .to_string();
                let mode = String::from_utf8_lossy(&h[100..107]).to_string();
                seen.push((
                    name,
                    h[156],
                    mode,
                    String::from_utf8_lossy(data).to_string(),
                    link,
                ));
            }
            i += 512 + size.div_ceil(512) * 512;
        }
        let long = format!("p/{}/deep.txt", "d".repeat(120));
        assert_eq!(
            seen,
            vec![
                (
                    "p/".into(),
                    b'5',
                    "0000755".into(),
                    String::new(),
                    String::new()
                ),
                (
                    "p/a.txt".into(),
                    b'0',
                    "0000644".into(),
                    "alpha\n".into(),
                    String::new()
                ),
                (
                    "p/run.sh".into(),
                    b'0',
                    "0000755".into(),
                    "#!/bin/sh\n".into(),
                    String::new()
                ),
                (
                    "p/link".into(),
                    b'2',
                    "0000777".into(),
                    String::new(),
                    "a.txt".into()
                ),
                (
                    "p/empty".into(),
                    b'0',
                    "0000644".into(),
                    String::new(),
                    String::new()
                ),
                (long, b'0', "0000644".into(), "deep\n".into(), String::new()),
            ]
        );
        assert!(tar[i..].iter().all(|b| *b == 0) && tar.len() - i >= 1024);
    }

    #[test]
    fn zip_round_trips_through_a_reader_and_is_deterministic() {
        let a = sample(Format::Zip);
        assert_eq!(a, sample(Format::Zip));
        let entries = bee_forge::packages::archive::zip_entries(&a, 1 << 20).unwrap();
        let got: Vec<(String, Vec<u8>)> = entries.into_iter().map(|e| (e.path, e.data)).collect();
        let long = format!("p/{}/deep.txt", "d".repeat(120));
        for (path, data) in [
            ("p/a.txt", b"alpha\n".to_vec()),
            ("p/run.sh", b"#!/bin/sh\n".to_vec()),
            ("p/link", b"a.txt".to_vec()),
            ("p/empty", Vec::new()),
            (long.as_str(), b"deep\n".to_vec()),
        ] {
            assert!(
                got.iter().any(|(p, d)| p == path && d == &data),
                "{path}: {got:?}"
            );
        }
        // External attributes carry the Unix modes.
        let modes: Vec<u32> = (0..a.len().saturating_sub(4))
            .filter(|&i| a[i..i + 4] == 0x0201_4b50u32.to_le_bytes())
            .map(|i| u32::from_le_bytes(a[i + 38..i + 42].try_into().unwrap()) >> 16)
            .collect();
        assert_eq!(
            modes,
            vec![0o040755, 0o100644, 0o100755, 0o120777, 0o100644, 0o100644]
        );
    }

    #[test]
    fn pax_records_count_their_own_length() {
        for n in [1usize, 5, 89, 90, 95, 990, 995] {
            let r = pax_record("path", "x".repeat(n).as_bytes());
            let (len, _) = std::str::from_utf8(&r).unwrap().split_once(' ').unwrap();
            assert_eq!(len.parse::<usize>().unwrap(), r.len(), "{n}");
        }
    }

    #[test]
    fn parents_are_listed_once_before_children() {
        let mut seen = std::collections::BTreeSet::new();
        assert_eq!(new_parents("p/a/b.txt", &mut seen), vec!["p", "p/a"]);
        assert_eq!(new_parents("p/a/c.txt", &mut seen), Vec::<String>::new());
        assert_eq!(new_parents("p/d/e.txt", &mut seen), vec!["p/d"]);
    }

    /// Mutational fuzzing of every archive reader the server uses (package and artifact
    /// uploads, GitHub migration archives) over archives this writer produced: random byte
    /// flips, insertions, deletions and truncations never panic a reader. Deterministic
    /// (a fixed xorshift seed), 3,000 inputs per format.
    #[test]
    fn archive_readers_never_panic_on_mutated_archives() {
        use bee_forge::packages::archive::{gunzip, tar_entries, unpack_artifact, zip_entries};
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(&sample(Format::TarGz)[..])
            .read_to_end(&mut tar)
            .unwrap();
        for seed in [sample(Format::TarGz), sample(Format::Zip), tar] {
            for _ in 0..3_000 {
                let mut b = seed.clone();
                for _ in 0..1 + next() % 8 {
                    let n = b.len().max(1);
                    let at = (next() as usize) % n;
                    match next() % 4 {
                        0 if !b.is_empty() => b[at] = next() as u8,
                        1 => b.insert(at.min(b.len()), next() as u8),
                        2 if !b.is_empty() => {
                            b.remove(at);
                        }
                        _ => b.truncate(at),
                    }
                }
                let _ = gunzip(&b, 1 << 20);
                let _ = tar_entries(&b, 1 << 20);
                let _ = zip_entries(&b, 1 << 20);
                let _ = unpack_artifact(&b, 1 << 20);
                let _ = bee_github::archive::read_tar(&b);
            }
        }
    }

    #[test]
    fn zip_limits_are_checked_up_front() {
        assert!(check_limits(Format::Zip, 70_000, 0).is_err());
        assert!(check_limits(Format::Zip, 10, 5 << 30).is_err());
        assert!(check_limits(Format::TarGz, 70_000, 5 << 30).is_ok());
        assert!(check_limits(Format::Zip, 10, 1 << 20).is_ok());
    }
}
