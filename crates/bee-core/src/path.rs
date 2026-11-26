//! Repository paths and tree entry names (SPEC §4.2, §4.5).
//!
//! Paths are bytes: no Unicode normalization, compared bytewise.

use std::fmt;

use crate::error::{BeeError, Result, Subcode};
use crate::limits::Limits;

/// The reserved name no tree entry may use, compared ASCII case-insensitively.
pub const RESERVED_NAME: &[u8] = b".bee";

/// The working-copy directory's name before the project was renamed from swarm. Still
/// reserved, so frozen vectors keep their meaning and an entry cannot collide with an
/// older checkout's metadata.
pub const LEGACY_RESERVED_NAME: &[u8] = b".swarm";

/// Checks one tree entry name (one path component) against SPEC §4.2: non-empty, at most
/// 255 bytes, not `.` or `..`, no `/` or NUL, and not `.bee` or `.swarm` in any ASCII case.
pub fn validate_name(name: &[u8]) -> Result<()> {
    name_problem(name).map_or(Ok(()), |why| {
        Err(BeeError::invalid(
            Subcode::InvalidName,
            format!("invalid name {:?}: {why}", String::from_utf8_lossy(name)),
        ))
    })
}

fn name_problem(name: &[u8]) -> Option<&'static str> {
    if name.is_empty() {
        Some("empty")
    } else if name.len() as u64 > Limits::DEFAULT.component_bytes {
        Some("longer than 255 bytes")
    } else if name == b"." || name == b".." {
        Some("`.` and `..` are not names")
    } else if name.contains(&b'/') {
        Some("contains `/`")
    } else if name.contains(&0) {
        Some("contains NUL")
    } else if name.eq_ignore_ascii_case(RESERVED_NAME) {
        Some("`.bee` is reserved")
    } else if name.eq_ignore_ascii_case(LEGACY_RESERVED_NAME) {
        Some("`.swarm` is reserved")
    } else {
        None
    }
}

/// A validated repository path: `/`-separated components, each a valid name, at most 4096
/// bytes and 256 components.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    /// Validates and wraps a path.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<RepoPath> {
        let bytes = bytes.into();
        let bad = |why: String| {
            BeeError::invalid(
                Subcode::InvalidPath,
                format!("invalid path {:?}: {why}", String::from_utf8_lossy(&bytes)),
            )
        };
        if bytes.is_empty() {
            return Err(bad("empty".into()));
        }
        let limits = Limits::DEFAULT;
        if bytes.len() as u64 > limits.path_bytes {
            return Err(bad(format!("longer than {} bytes", limits.path_bytes)));
        }
        let mut depth = 0u64;
        for component in bytes.split(|&b| b == b'/') {
            depth += 1;
            if let Some(why) = name_problem(component) {
                return Err(bad(format!("component {depth}: {why}")));
            }
        }
        if depth > limits.path_depth {
            return Err(bad(format!("deeper than {} components", limits.path_depth)));
        }
        Ok(RepoPath(bytes))
    }

    /// Joins validated components with `/`.
    pub fn from_components<'a, I>(components: I) -> Result<RepoPath>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let mut out = Vec::new();
        for (i, c) in components.into_iter().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(c);
        }
        RepoPath::new(out)
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the path, returning its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The components, in order.
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|&b| b == b'/')
    }

    /// Number of components.
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The last component.
    pub fn file_name(&self) -> &[u8] {
        match self.0.iter().rposition(|&b| b == b'/') {
            Some(i) => &self.0[i + 1..],
            None => &self.0,
        }
    }

    /// The parent directory, or `None` for a top-level path.
    pub fn parent(&self) -> Option<RepoPath> {
        let i = self.0.iter().rposition(|&b| b == b'/')?;
        Some(RepoPath(self.0[..i].to_vec()))
    }

    /// Every proper ancestor directory, outermost first.
    pub fn ancestors(&self) -> Vec<RepoPath> {
        self.0
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'/')
            .map(|(i, _)| RepoPath(self.0[..i].to_vec()))
            .collect()
    }

    /// This path with one more component.
    pub fn join(&self, name: &[u8]) -> Result<RepoPath> {
        let mut out = self.0.clone();
        out.push(b'/');
        out.extend_from_slice(name);
        RepoPath::new(out)
    }

    /// True when `other` lies strictly inside this path, treated as a directory.
    pub fn is_ancestor_of(&self, other: &RepoPath) -> bool {
        other.0.len() > self.0.len()
            && other.0.starts_with(&self.0)
            && other.0[self.0.len()] == b'/'
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", String::from_utf8_lossy(&self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn name_rules() {
        for good in [
            &b"a"[..],
            b"README.md",
            b"...",
            b".beex",
            b"x.bee",
            &[0xff, 0xfe],
            b" ",
        ] {
            assert!(validate_name(good).is_ok(), "{good:?}");
        }
        for bad in [
            &b""[..],
            b".",
            b"..",
            b"a/b",
            b"a\0b",
            b".bee",
            b".BEE",
            b".BeE",
            b".swarm",
            b".SwArM",
        ] {
            assert!(
                validate_name(bad)
                    .unwrap_err()
                    .is_invalid(Subcode::InvalidName),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn component_max_255_bytes() {
        assert!(validate_name(&[b'a'; 255]).is_ok());
        assert!(validate_name(&[b'a'; 256]).is_err());
        let mut p = b"dir/".to_vec();
        p.extend_from_slice(&[b'a'; 256]);
        assert!(
            RepoPath::new(p)
                .unwrap_err()
                .is_invalid(Subcode::InvalidPath)
        );
    }

    #[test]
    fn path_rules() {
        for good in [&b"a"[..], b"a/b/c", b"src/.beerc", b"\xc3\xa9/x"] {
            assert!(RepoPath::new(good).is_ok(), "{good:?}");
        }
        for bad in [
            &b""[..],
            b"/a",
            b"a/",
            b"a//b",
            b"./a",
            b"a/../b",
            b"a/./b",
            b"a/.bee/b",
            b".Bee",
            b"a\0",
            b"/",
        ] {
            assert!(
                RepoPath::new(bad)
                    .unwrap_err()
                    .is_invalid(Subcode::InvalidPath),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn path_length_and_depth_limits() {
        let deep = vec!["a"; 256].join("/");
        assert!(RepoPath::new(deep.clone()).is_ok());
        assert!(RepoPath::new(format!("{deep}/a")).is_err());
        let comp = "b".repeat(200);
        let parts = vec![comp.as_str(); 20];
        let long = parts.join("/"); // 20*200 + 19 = 4019 bytes
        let exact = format!("{long}/{}", "c".repeat(76));
        assert_eq!(exact.len(), 4096);
        assert!(RepoPath::new(exact.clone()).is_ok(), "exactly 4096");
        assert!(RepoPath::new(format!("{exact}c")).is_err(), "4097");
    }

    #[test]
    fn path_accessors() {
        let p = RepoPath::new("a/b/c.txt").unwrap();
        assert_eq!(p.depth(), 3);
        assert_eq!(p.file_name(), b"c.txt");
        assert_eq!(p.parent().unwrap().as_bytes(), b"a/b");
        assert_eq!(RepoPath::new("a").unwrap().parent(), None);
        assert_eq!(RepoPath::new("a").unwrap().file_name(), b"a");
        let anc: Vec<_> = p.ancestors().into_iter().map(|a| a.to_string()).collect();
        assert_eq!(anc, ["a", "a/b"]);
        assert_eq!(
            p.components().collect::<Vec<_>>(),
            [&b"a"[..], b"b", b"c.txt"]
        );
        let a = RepoPath::new("a").unwrap();
        assert!(a.is_ancestor_of(&p));
        assert!(!p.is_ancestor_of(&a));
        assert!(!a.is_ancestor_of(&a));
        assert!(!a.is_ancestor_of(&RepoPath::new("ab/c").unwrap()));
        assert_eq!(a.join(b"x").unwrap().as_bytes(), b"a/x");
        assert!(a.join(b"..").is_err());
        assert_eq!(
            RepoPath::from_components([&b"x"[..], b"y"]).unwrap(),
            RepoPath::new("x/y").unwrap()
        );
        assert!(RepoPath::from_components(std::iter::empty()).is_err());
        assert_eq!(p.to_string(), "a/b/c.txt");
        assert_eq!(format!("{a:?}"), "RepoPath(\"a\")");
        assert_eq!(p.clone().into_bytes(), b"a/b/c.txt");
    }

    #[test]
    fn paths_sort_bytewise() {
        let mut v = [
            RepoPath::new("a/x").unwrap(),
            RepoPath::new("a-b").unwrap(),
            RepoPath::new("a").unwrap(),
        ];
        v.sort();
        let s: Vec<_> = v.iter().map(|p| p.to_string()).collect();
        assert_eq!(s, ["a", "a-b", "a/x"]);
    }

    proptest! {
        #[test]
        fn valid_components_make_valid_paths(
            comps in proptest::collection::vec("[a-zA-Z0-9_.-]{1,20}", 1..10)
        ) {
            let ok = comps.iter().all(|c| validate_name(c.as_bytes()).is_ok());
            let joined = comps.join("/");
            prop_assert_eq!(RepoPath::new(joined.clone()).is_ok(), ok);
            if ok {
                let p = RepoPath::new(joined).unwrap();
                prop_assert_eq!(p.depth(), comps.len());
            }
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            if let Ok(p) = RepoPath::new(bytes.clone()) {
                prop_assert!(p.components().all(|c| validate_name(c).is_ok()));
                prop_assert_eq!(p.as_bytes(), &bytes[..]);
            }
        }
    }
}
