//! The base manifest: every leaf of the checked-out changeset inside the sparse scope, as
//! `path -> (mode, blob id, size)`, kept in `.bee/manifest` as a sealed, versioned binary
//! file. Status and commit compare the working tree with it without touching the network.

use std::collections::BTreeMap;

use bee_core::{Mode, ObjectId};
use serde::{Deserialize, Serialize};

use crate::error::{Result, WcError};
use crate::fsutil::{Reader, put_bytes, seal, unseal};

const MAGIC: &[u8; 4] = b"SWMF";
const VERSION: u32 = 1;

/// One leaf: its mode, content id and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// File, executable or symlink (gitlinks are kept but never written to disk).
    pub mode: Mode,
    /// Blob id (or foreign id for a gitlink).
    pub id: ObjectId,
    /// Blob length.
    pub size: u64,
}

impl Entry {
    /// A file entry for `bytes` with `mode`.
    pub fn for_bytes(mode: Mode, bytes: &[u8]) -> Self {
        Self {
            mode,
            id: bee_object::blob_id(bytes),
            size: bytes.len() as u64,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct EntryJson {
    mode: u64,
    id: String,
    size: u64,
}

impl Serialize for Entry {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        EntryJson {
            mode: self.mode.as_u64(),
            id: self.id.to_text(),
            size: self.size,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let j = EntryJson::deserialize(d)?;
        Ok(Entry {
            mode: Mode::from_u64(j.mode).map_err(serde::de::Error::custom)?,
            id: ObjectId::parse(&j.id).map_err(serde::de::Error::custom)?,
            size: j.size,
        })
    }
}

/// Leaves by path, in bytewise path order.
pub type Manifest = BTreeMap<Vec<u8>, Entry>;

/// One path that differs between two manifests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The path.
    pub path: Vec<u8>,
    /// Its entry before (`None` = absent).
    pub old: Option<Entry>,
    /// Its entry after (`None` = absent).
    pub new: Option<Entry>,
}

/// Every path whose entry differs between `a` and `b`, in bytewise path order.
pub fn diff(a: &Manifest, b: &Manifest) -> Vec<Change> {
    let mut out = Vec::new();
    let mut ia = a.iter().peekable();
    let mut ib = b.iter().peekable();
    loop {
        match (ia.peek(), ib.peek()) {
            (None, None) => break,
            (Some((pa, ea)), None) => {
                out.push(Change {
                    path: (*pa).clone(),
                    old: Some(**ea),
                    new: None,
                });
                ia.next();
            }
            (None, Some((pb, eb))) => {
                out.push(Change {
                    path: (*pb).clone(),
                    old: None,
                    new: Some(**eb),
                });
                ib.next();
            }
            (Some((pa, ea)), Some((pb, eb))) => match pa.cmp(pb) {
                std::cmp::Ordering::Less => {
                    out.push(Change {
                        path: (*pa).clone(),
                        old: Some(**ea),
                        new: None,
                    });
                    ia.next();
                }
                std::cmp::Ordering::Greater => {
                    out.push(Change {
                        path: (*pb).clone(),
                        old: None,
                        new: Some(**eb),
                    });
                    ib.next();
                }
                std::cmp::Ordering::Equal => {
                    if ea != eb {
                        out.push(Change {
                            path: (*pa).clone(),
                            old: Some(**ea),
                            new: Some(**eb),
                        });
                    }
                    ia.next();
                    ib.next();
                }
            },
        }
    }
    out
}

/// Applies changes (`new = None` deletes) to a manifest.
pub fn apply(manifest: &mut Manifest, changes: &[(Vec<u8>, Option<Entry>)]) {
    for (path, entry) in changes {
        match entry {
            Some(e) => {
                manifest.insert(path.clone(), *e);
            }
            None => {
                manifest.remove(path);
            }
        }
    }
}

/// Encodes a manifest as a sealed file.
pub fn encode(manifest: &Manifest) -> Vec<u8> {
    let mut out = Vec::with_capacity(manifest.len() * 64);
    out.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    for (path, e) in manifest {
        put_bytes(&mut out, path);
        out.push(e.mode.as_u64() as u8);
        out.extend_from_slice(e.id.as_bytes());
        out.extend_from_slice(&e.size.to_le_bytes());
    }
    seal(MAGIC, VERSION, &out)
}

/// Decodes a sealed manifest file.
pub fn decode(bytes: &[u8]) -> Result<Manifest> {
    let (version, payload) = unseal(MAGIC, "manifest", bytes)?;
    if version != VERSION {
        return Err(WcError::integrity(
            "unknown_version",
            format!("manifest version {version} is not supported"),
        ));
    }
    let mut r = Reader::new(payload, "manifest");
    let n = r.u64()?;
    let mut out = Manifest::new();
    let mut last: Option<Vec<u8>> = None;
    for _ in 0..n {
        let path = r.bytes()?;
        if last.as_ref().is_some_and(|l| *l >= path) {
            return Err(WcError::integrity(
                "corrupt_file",
                "manifest: paths out of order",
            ));
        }
        let mode = Mode::from_u64(u64::from(r.u8()?))
            .map_err(|e| WcError::integrity("corrupt_file", e.to_string()))?;
        let id = ObjectId::from_bytes(r.raw(34)?)
            .map_err(|e| WcError::integrity("corrupt_file", e.to_string()))?;
        let size = r.u64()?;
        last = Some(path.clone());
        out.insert(path, Entry { mode, id, size });
    }
    r.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(text: &str) -> Entry {
        Entry::for_bytes(Mode::File, text.as_bytes())
    }

    #[test]
    fn roundtrip_and_diff() {
        let mut a = Manifest::new();
        a.insert(b"a.txt".to_vec(), e("a"));
        a.insert(b"dir/b".to_vec(), e("b"));
        a.insert(b"gone".to_vec(), e("g"));
        let decoded = decode(&encode(&a)).expect("decode");
        assert_eq!(decoded, a);
        let mut b = a.clone();
        b.insert(b"dir/b".to_vec(), e("b2"));
        b.remove(b"gone".as_slice());
        b.insert(b"new".to_vec(), e("n"));
        let d = diff(&a, &b);
        let paths: Vec<&[u8]> = d.iter().map(|c| c.path.as_slice()).collect();
        assert_eq!(paths, vec![&b"dir/b"[..], b"gone", b"new"]);
        assert!(d[1].new.is_none() && d[2].old.is_none());
        let mut c = a.clone();
        apply(
            &mut c,
            &d.iter()
                .map(|x| (x.path.clone(), x.new))
                .collect::<Vec<_>>(),
        );
        assert_eq!(c, b);
    }

    #[test]
    fn entry_json_roundtrip() {
        let entry = Entry::for_bytes(Mode::Executable, b"#!/bin/sh\n");
        let text = serde_json::to_string(&entry).expect("ser");
        assert!(text.contains("\"mode\":2"), "{text}");
        let back: Entry = serde_json::from_str(&text).expect("de");
        assert_eq!(back, entry);
    }
}
