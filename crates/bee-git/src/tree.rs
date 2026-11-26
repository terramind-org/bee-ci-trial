//! Git trees: `(<octal mode> SP <name> NUL <raw id>)*`.
//!
//! The mode is kept as the exact digits found (`100664`, `040000` and other non-canonical
//! spellings survive a round trip), and entries keep their order, so serialization is
//! byte-exact even for trees git itself would no longer write.

use std::cmp::Ordering;

use bee_core::{BeeError, Mode, Result, Subcode};

use crate::error::malformed;
use crate::oid::{GitOid, ObjectFormat};

/// What a tree entry points at, decided by the mode's type bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    /// Regular file (`100644`; also non-canonical regular modes without the owner exec bit).
    File,
    /// Executable file (`100755`; any regular mode with the owner exec bit).
    Executable,
    /// Symbolic link (`120000`).
    Symlink,
    /// Subtree (`40000`).
    Tree,
    /// Submodule commit (`160000`).
    Gitlink,
}

impl EntryKind {
    /// The mode git writes today for this kind.
    pub fn canonical_mode(self) -> &'static [u8] {
        match self {
            EntryKind::File => b"100644",
            EntryKind::Executable => b"100755",
            EntryKind::Symlink => b"120000",
            EntryKind::Tree => b"40000",
            EntryKind::Gitlink => b"160000",
        }
    }

    /// The matching bee mode (SPEC §4.2).
    pub fn to_bee(self) -> Mode {
        match self {
            EntryKind::File => Mode::File,
            EntryKind::Executable => Mode::Executable,
            EntryKind::Symlink => Mode::Symlink,
            EntryKind::Tree => Mode::Tree,
            EntryKind::Gitlink => Mode::Gitlink,
        }
    }

    /// The kind for a bee mode.
    pub fn from_bee(mode: Mode) -> EntryKind {
        match mode {
            Mode::File => EntryKind::File,
            Mode::Executable => EntryKind::Executable,
            Mode::Symlink => EntryKind::Symlink,
            Mode::Tree => EntryKind::Tree,
            Mode::Gitlink => EntryKind::Gitlink,
        }
    }

    /// Classifies raw mode digits: octal, 1 to 8 digits, with a regular-file, symlink,
    /// directory or gitlink type. Anything else is `INVALID_ARGUMENT`/`INVALID_MODE`.
    pub fn from_mode(mode: &[u8]) -> Result<EntryKind> {
        let bad = |why: &str| {
            BeeError::invalid(
                Subcode::InvalidMode,
                format!(
                    "git mode {:?}: {why}",
                    String::from_utf8_lossy(mode).into_owned()
                ),
            )
        };
        if mode.is_empty() || mode.len() > 8 {
            return Err(bad("must be 1 to 8 octal digits"));
        }
        let mut v: u32 = 0;
        for &c in mode {
            if !(b'0'..=b'7').contains(&c) {
                return Err(bad("not octal"));
            }
            v = (v << 3) | u32::from(c - b'0');
        }
        match v & 0o170000 {
            0o100000 if v & 0o100 != 0 => Ok(EntryKind::Executable),
            0o100000 => Ok(EntryKind::File),
            0o120000 => Ok(EntryKind::Symlink),
            0o040000 => Ok(EntryKind::Tree),
            0o160000 => Ok(EntryKind::Gitlink),
            _ => Err(bad("unknown object type bits")),
        }
    }
}

/// One tree entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitTreeEntry {
    /// The mode digits exactly as stored.
    pub mode: Vec<u8>,
    /// The entry name (non-empty, no NUL, no `/`).
    pub name: Vec<u8>,
    /// The id the entry points at.
    pub oid: GitOid,
}

impl GitTreeEntry {
    /// An entry with the canonical mode for `kind`.
    pub fn new(kind: EntryKind, name: impl Into<Vec<u8>>, oid: GitOid) -> GitTreeEntry {
        GitTreeEntry {
            mode: kind.canonical_mode().to_vec(),
            name: name.into(),
            oid,
        }
    }

    /// The entry's kind, from its mode.
    pub fn kind(&self) -> Result<EntryKind> {
        EntryKind::from_mode(&self.mode)
    }

    /// True when the mode is spelled the way git writes it today.
    pub fn has_canonical_mode(&self) -> bool {
        self.kind()
            .is_ok_and(|k| k.canonical_mode() == self.mode.as_slice())
    }
}

/// Git's tree order: names compared bytewise as if every directory name ended in `/`.
pub fn git_order(a_name: &[u8], a_is_dir: bool, b_name: &[u8], b_is_dir: bool) -> Ordering {
    let n = a_name.len().min(b_name.len());
    match a_name[..n].cmp(&b_name[..n]) {
        Ordering::Equal => {}
        other => return other,
    }
    let next = |name: &[u8], is_dir: bool| -> Option<u8> {
        match name.get(n) {
            Some(&c) => Some(c),
            None if is_dir => Some(b'/'),
            None => None,
        }
    };
    next(a_name, a_is_dir).cmp(&next(b_name, b_is_dir))
}

/// A git tree.
#[derive(Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct GitTree {
    /// The entries, in stored order.
    pub entries: Vec<GitTreeEntry>,
}

impl GitTree {
    /// Parses a tree body. Each entry needs octal mode digits, one space, a non-empty name
    /// without `/`, NUL, and a full raw id; nothing may follow the last entry.
    pub fn parse(data: &[u8], format: ObjectFormat) -> Result<GitTree> {
        let n = format.oid_len();
        let mut entries = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let space = rest
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| malformed("tree", "entry without a space"))?;
            let mode = &rest[..space];
            if mode.is_empty() || !mode.iter().all(|c| (b'0'..=b'7').contains(c)) {
                return Err(malformed("tree", "mode is not octal digits"));
            }
            let after = &rest[space + 1..];
            let nul = after
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| malformed("tree", "entry name without NUL"))?;
            let name = &after[..nul];
            if name.is_empty() || name.contains(&b'/') {
                return Err(malformed("tree", "entry name empty or contains '/'"));
            }
            let id_bytes = after
                .get(nul + 1..nul + 1 + n)
                .ok_or_else(|| malformed("tree", "truncated entry id"))?;
            entries.push(GitTreeEntry {
                mode: mode.to_vec(),
                name: name.to_vec(),
                oid: format.oid_from_bytes(id_bytes)?,
            });
            rest = &after[nul + 1 + n..];
        }
        Ok(GitTree { entries })
    }

    /// The body bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend_from_slice(&e.mode);
            out.push(b' ');
            out.extend_from_slice(&e.name);
            out.push(0);
            out.extend_from_slice(e.oid.as_bytes());
        }
        out
    }

    /// Sorts entries into git's order (directories compare as if suffixed with `/`).
    /// Entries with an unparseable mode sort as non-directories.
    pub fn sort(&mut self) {
        self.entries
            .sort_by(|a, b| git_order(&a.name, is_dir(a), &b.name, is_dir(b)));
    }

    /// True when entries are strictly in git's order (so also free of duplicates).
    pub fn is_sorted(&self) -> bool {
        self.entries.windows(2).all(|w| {
            git_order(&w[0].name, is_dir(&w[0]), &w[1].name, is_dir(&w[1])) == Ordering::Less
        })
    }
}

fn is_dir(e: &GitTreeEntry) -> bool {
    e.kind() == Ok(EntryKind::Tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> GitOid {
        GitOid::Sha1([b; 20])
    }

    #[test]
    fn mode_classification() {
        let cases: [(&[u8], EntryKind); 9] = [
            (b"100644", EntryKind::File),
            (b"100664", EntryKind::File),
            (b"100600", EntryKind::File),
            (b"100755", EntryKind::Executable),
            (b"100744", EntryKind::Executable),
            (b"120000", EntryKind::Symlink),
            (b"40000", EntryKind::Tree),
            (b"040000", EntryKind::Tree),
            (b"160000", EntryKind::Gitlink),
        ];
        for (m, k) in cases {
            assert_eq!(EntryKind::from_mode(m).unwrap(), k, "{m:?}");
            assert_eq!(EntryKind::from_bee(k.to_bee()), k);
        }
        for bad in [&b""[..], b"8", b"644", b"000000", b"1006440000", b"10064x"] {
            assert!(
                EntryKind::from_mode(bad)
                    .unwrap_err()
                    .is_invalid(Subcode::InvalidMode),
                "{bad:?}"
            );
        }
        let e = GitTreeEntry::new(EntryKind::Tree, "d", oid(1));
        assert!(e.has_canonical_mode());
        let odd = GitTreeEntry {
            mode: b"040000".to_vec(),
            ..e
        };
        assert!(!odd.has_canonical_mode());
    }

    #[test]
    fn git_ordering_suffixes_directories() {
        // "a.c" < "a/" < "a0": a directory named "a" sorts between them.
        assert_eq!(git_order(b"a.c", false, b"a", true), Ordering::Less);
        assert_eq!(git_order(b"a", true, b"a0", false), Ordering::Less);
        assert_eq!(git_order(b"a", false, b"a.c", false), Ordering::Less);
        assert_eq!(git_order(b"a", false, b"a", true), Ordering::Less);
        assert_eq!(git_order(b"a", true, b"a", true), Ordering::Equal);
        let mut t = GitTree {
            entries: vec![
                GitTreeEntry::new(EntryKind::File, "a0", oid(1)),
                GitTreeEntry::new(EntryKind::Tree, "a", oid(2)),
                GitTreeEntry::new(EntryKind::File, "a.c", oid(3)),
            ],
        };
        assert!(!t.is_sorted());
        t.sort();
        assert!(t.is_sorted());
        let names: Vec<&[u8]> = t.entries.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(names, [&b"a.c"[..], b"a", b"a0"]);
    }

    #[test]
    fn parse_serialize_exact() {
        let t = GitTree {
            entries: vec![
                GitTreeEntry {
                    mode: b"100664".to_vec(),
                    name: b"\xff\xfe".to_vec(),
                    oid: oid(9),
                },
                GitTreeEntry::new(EntryKind::Tree, "z", oid(8)),
            ],
        };
        let bytes = t.serialize();
        assert_eq!(GitTree::parse(&bytes, ObjectFormat::Sha1).unwrap(), t);
        assert!(GitTree::parse(&bytes, ObjectFormat::Sha256).is_err());
        assert!(GitTree::parse(&bytes[..bytes.len() - 1], ObjectFormat::Sha1).is_err());
        assert!(GitTree::parse(b"100644 \0", ObjectFormat::Sha1).is_err());
        assert!(GitTree::parse(b"10a644 x\0", ObjectFormat::Sha1).is_err());
        assert!(GitTree::parse(b"100644 a/b\0", ObjectFormat::Sha1).is_err());
        assert!(GitTree::parse(b"100644x", ObjectFormat::Sha1).is_err());
        assert_eq!(
            GitTree::parse(b"", ObjectFormat::Sha1).unwrap(),
            GitTree::default()
        );
    }
}
