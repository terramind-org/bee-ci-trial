//! Readers for the archive formats the registries and Pages receive: ustar/pax tar (a gem is a
//! tar, a Pages artifact is a tar.gz), gzip, and zip (a nupkg, a Pages artifact zip).
//!
//! Every reader bounds the bytes it inflates and rejects paths that escape the archive root.
//! Zip64 and encrypted entries are refused rather than misread.

use std::io::Read;

use flate2::Crc;
use flate2::read::{DeflateDecoder, GzDecoder};

use crate::error::{ForgeError, Result};

/// One file of an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Normalized path, `/`-separated, no leading slash, never containing `..`.
    pub path: String,
    /// Content.
    pub data: Vec<u8>,
}

fn bad(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_archive", msg)
}

/// Normalizes an archive path; `None` for a path that is absolute, escapes the root or is empty.
pub fn clean_path(raw: &str) -> Option<String> {
    let raw = raw.replace('\\', "/");
    let mut parts = Vec::new();
    for p in raw.split('/') {
        match p {
            "" | "." => {}
            ".." => return None,
            p if p.contains('\0') => return None,
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Inflates a gzip stream, failing with `TOO_LARGE` beyond `max` bytes.
pub fn gunzip(data: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    GzDecoder::new(data)
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| bad(format!("not a gzip stream: {e}")))?;
    if out.len() > max {
        return Err(ForgeError::too_large(
            "inflated size",
            max as u64,
            out.len() as u64,
        ));
    }
    Ok(out)
}

/// Compresses `data` with gzip.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

fn octal(field: &[u8]) -> Option<u64> {
    let s: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s, 8).ok()
}

fn cstr(field: &[u8]) -> String {
    String::from_utf8_lossy(&field[..field.iter().position(|b| *b == 0).unwrap_or(field.len())])
        .into_owned()
}

/// Regular files of a tar archive (ustar, GNU long names and pax `path` records). Directories,
/// links and other special entries are skipped. Total content is bounded by `max_total`.
pub fn tar_entries(data: &[u8], max_total: usize) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut total = 0usize;
    let mut long_name: Option<String> = None;
    let mut pax_path: Option<String> = None;
    while pos + 512 <= data.len() {
        let h = &data[pos..pos + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let size = octal(&h[124..136]).ok_or_else(|| bad("bad tar size field"))? as usize;
        let kind = h[156];
        let body_start = pos + 512;
        let body_end = body_start
            .checked_add(size)
            .filter(|e| *e <= data.len())
            .ok_or_else(|| bad("tar entry runs past the end of the archive"))?;
        let body = &data[body_start..body_end];
        pos = body_start + size.div_ceil(512) * 512;
        match kind {
            b'L' => {
                long_name = Some(cstr(body));
                continue;
            }
            b'x' => {
                let text = String::from_utf8_lossy(body);
                for rec in text.split('\n') {
                    if let Some((_, kv)) = rec.split_once(' ')
                        && let Some(v) = kv.strip_prefix("path=")
                    {
                        pax_path = Some(v.to_string());
                    }
                }
                continue;
            }
            b'g' => continue,
            _ => {}
        }
        let mut name = cstr(&h[0..100]);
        if &h[257..262] == b"ustar" {
            let prefix = cstr(&h[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        if let Some(n) = long_name.take() {
            name = n;
        }
        if let Some(n) = pax_path.take() {
            name = n;
        }
        if kind != b'0' && kind != 0 && kind != b'7' {
            continue;
        }
        let Some(path) = clean_path(&name) else {
            return Err(bad(format!("unsafe path `{name}` in archive")));
        };
        total += body.len();
        if total > max_total {
            return Err(ForgeError::too_large(
                "archive content",
                max_total as u64,
                total as u64,
            ));
        }
        out.push(Entry {
            path,
            data: body.to_vec(),
        });
    }
    Ok(out)
}

fn le16(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn le32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// Files of a zip archive (stored and deflated entries, CRC-32 verified). Directory entries are
/// skipped. Total inflated content is bounded by `max_total`.
pub fn zip_entries(data: &[u8], max_total: usize) -> Result<Vec<Entry>> {
    let trunc = || bad("zip archive is truncated or corrupt");
    let start = data.len().saturating_sub(22 + 65_535);
    let eocd = (start..data.len().saturating_sub(21))
        .rev()
        .find(|&i| data[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or_else(|| bad("not a zip archive (no end-of-directory record)"))?;
    let count = le16(data, eocd + 10).ok_or_else(trunc)?;
    let cd_offset = le32(data, eocd + 16).ok_or_else(trunc)?;
    if count == 0xffff || cd_offset == 0xffff_ffff {
        return Err(bad("zip64 archives are not supported"));
    }
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut p = cd_offset;
    for _ in 0..count {
        if data.get(p..p + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err(trunc());
        }
        let flags = le16(data, p + 8).ok_or_else(trunc)?;
        let method = le16(data, p + 10).ok_or_else(trunc)?;
        let crc = le32(data, p + 16).ok_or_else(trunc)? as u32;
        let csize = le32(data, p + 20).ok_or_else(trunc)?;
        let usize_ = le32(data, p + 24).ok_or_else(trunc)?;
        let nlen = le16(data, p + 28).ok_or_else(trunc)?;
        let elen = le16(data, p + 30).ok_or_else(trunc)?;
        let clen = le16(data, p + 32).ok_or_else(trunc)?;
        let local = le32(data, p + 42).ok_or_else(trunc)?;
        let name = String::from_utf8_lossy(data.get(p + 46..p + 46 + nlen).ok_or_else(trunc)?)
            .into_owned();
        p += 46 + nlen + elen + clen;
        if name.ends_with('/') {
            continue;
        }
        if flags & 1 != 0 {
            return Err(bad("encrypted zip entries are not supported"));
        }
        let Some(path) = clean_path(&name) else {
            return Err(bad(format!("unsafe path `{name}` in archive")));
        };
        if data.get(local..local + 4) != Some(&[0x50, 0x4b, 0x03, 0x04]) {
            return Err(trunc());
        }
        let lname = le16(data, local + 26).ok_or_else(trunc)?;
        let lextra = le16(data, local + 28).ok_or_else(trunc)?;
        let body_at = local + 30 + lname + lextra;
        let raw = data.get(body_at..body_at + csize).ok_or_else(trunc)?;
        total = total.saturating_add(usize_);
        if total > max_total {
            return Err(ForgeError::too_large(
                "archive content",
                max_total as u64,
                total as u64,
            ));
        }
        let content = match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(usize_.min(max_total));
                DeflateDecoder::new(raw)
                    .take(usize_ as u64 + 1)
                    .read_to_end(&mut v)
                    .map_err(|e| bad(format!("corrupt deflate stream in `{name}`: {e}")))?;
                v
            }
            m => return Err(bad(format!("unsupported zip compression method {m}"))),
        };
        if content.len() != usize_ {
            return Err(bad(format!("size mismatch for `{name}`")));
        }
        let mut c = Crc::new();
        c.update(&content);
        if c.sum() != crc {
            return Err(bad(format!("CRC mismatch for `{name}`")));
        }
        out.push(Entry {
            path,
            data: content,
        });
    }
    Ok(out)
}

/// Unpacks a Pages or workflow artifact: zip, tar.gz or plain tar, detected by content.
pub fn unpack_artifact(data: &[u8], max_total: usize) -> Result<Vec<Entry>> {
    if data.starts_with(&[0x50, 0x4b]) {
        zip_entries(data, max_total)
    } else if data.starts_with(&[0x1f, 0x8b]) {
        tar_entries(&gunzip(data, max_total)?, max_total)
    } else {
        tar_entries(data, max_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, body) in files {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..107].copy_from_slice(b"0000644");
            let size = format!("{:011o}", body.len());
            h[124..135].copy_from_slice(size.as_bytes());
            h[156] = b'0';
            h[257..262].copy_from_slice(b"ustar");
            out.extend_from_slice(&h);
            out.extend_from_slice(body);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    fn zip_stored(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, body) in files {
            let mut c = Crc::new();
            c.update(body);
            let off = out.len() as u32;
            out.extend_from_slice(&[0x50, 0x4b, 3, 4, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&c.sum().to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);
            cd.extend_from_slice(&[0x50, 0x4b, 1, 2, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            cd.extend_from_slice(&c.sum().to_le_bytes());
            cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0u8; 12]);
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&[0x50, 0x4b, 5, 6, 0, 0, 0, 0]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn tar_round_trip_and_traversal() {
        let t = tar(&[("a/b.txt", b"hello"), ("c.txt", &[7u8; 700])]);
        let e = tar_entries(&t, 1 << 20).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].path, "a/b.txt");
        assert_eq!(e[1].data.len(), 700);
        assert!(tar_entries(&tar(&[("../evil", b"x")]), 1 << 20).is_err());
        assert!(tar_entries(&t, 10).is_err());
        let gz = gzip(&t);
        assert_eq!(unpack_artifact(&gz, 1 << 20).unwrap(), e);
    }

    #[test]
    fn zip_round_trip_and_crc() {
        let z = zip_stored(&[("x/y.nuspec", b"<package/>"), ("lib/a.dll", b"MZ")]);
        let e = zip_entries(&z, 1 << 20).unwrap();
        assert_eq!(e[0].path, "x/y.nuspec");
        assert_eq!(e[1].data, b"MZ");
        let mut bad_crc = z.clone();
        let at = bad_crc.iter().position(|b| *b == b'M').unwrap();
        bad_crc[at] = b'N';
        assert!(zip_entries(&bad_crc, 1 << 20).is_err());
        assert!(zip_entries(b"not a zip at all, really not", 100).is_err());
    }

    #[test]
    fn paths_are_cleaned() {
        assert_eq!(clean_path("/a//b/./c").as_deref(), Some("a/b/c"));
        assert_eq!(clean_path("a/../b"), None);
        assert_eq!(clean_path(""), None);
    }
}
