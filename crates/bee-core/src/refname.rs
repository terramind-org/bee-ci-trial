//! Ref names (SPEC §5, "Ref name grammar").
//!
//! A ref name is 1 to 255 bytes of UTF-8, `/`-separated into non-empty components, where:
//! - no component starts with `.` or ends with `.` or `.lock`;
//! - the name contains no `..`, no ASCII control character or space, and none of
//!   `~ ^ : ? * [ \ @` (these are revision syntax or glob characters);
//! - the whole name is not id-shaped: an id tag (`b1`, `t1`, `c1`, `m1`, `g2`, `d1xx`, `k`),
//!   `_`, then only lowercase hex digits. Those spell ids and id prefixes in revisions.

use std::fmt;
use std::str::FromStr;

use crate::error::{BeeError, Result, Subcode};
use crate::hex;
use crate::limits::Limits;

/// Characters a ref name may not contain, besides controls and space.
pub const FORBIDDEN_REF_CHARS: &[char] = &['~', '^', ':', '?', '*', '[', '\\', '@'];

/// A validated ref name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefName(String);

/// Which namespace a ref belongs to (SPEC §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefNamespace {
    /// `main` or `release/*`: written only by Land.
    Protected,
    /// `ws/<principal>/<name>`: a principal's workspace.
    Workspace,
    /// `green/*`: tested tips marked by CI.
    Green,
    /// Anything else.
    Other,
}

impl RefName {
    /// Validates and wraps a ref name.
    pub fn new(name: impl Into<String>) -> Result<RefName> {
        let name = name.into();
        match problem(&name) {
            None => Ok(RefName(name)),
            Some(why) => Err(BeeError::invalid(
                Subcode::InvalidRefName,
                format!("invalid ref name {name:?}: {why}"),
            )),
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The namespace.
    pub fn namespace(&self) -> RefNamespace {
        let n = self.0.as_str();
        if n == "main" || n.starts_with("release/") {
            RefNamespace::Protected
        } else if n.starts_with("green/") {
            RefNamespace::Green
        } else if n.starts_with("ws/") && n.split('/').count() >= 3 {
            RefNamespace::Workspace
        } else {
            RefNamespace::Other
        }
    }

    /// For a workspace ref `ws/<principal>/<name>`, the principal component.
    pub fn workspace_owner(&self) -> Option<&str> {
        if self.namespace() == RefNamespace::Workspace {
            self.0.split('/').nth(1)
        } else {
            None
        }
    }
}

fn problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty");
    }
    if name.len() as u64 > Limits::DEFAULT.ref_name_bytes {
        return Some("longer than 255 bytes");
    }
    if name.contains("..") {
        return Some("contains `..`");
    }
    if name.chars().any(|c| c.is_ascii_control() || c == ' ') {
        return Some("contains a control character or space");
    }
    if name.contains(FORBIDDEN_REF_CHARS) {
        return Some("contains one of ~ ^ : ? * [ \\ @");
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Some("empty component (leading, trailing or doubled `/`)");
        }
        if component.starts_with('.') {
            return Some("component starts with `.`");
        }
        if component.ends_with('.') {
            return Some("component ends with `.`");
        }
        if component.ends_with(".lock") {
            return Some("component ends with `.lock`");
        }
    }
    if is_id_shaped(name) {
        return Some("looks like an object id, id prefix or change id");
    }
    None
}

pub(crate) fn is_id_shaped(name: &str) -> bool {
    let Some((tag, digits)) = name.split_once('_') else {
        return false;
    };
    let tag_ok = matches!(tag, "b1" | "t1" | "c1" | "m1" | "g2" | "k")
        || (tag.len() == 4 && tag.starts_with("d1") && hex::is_lower_hex(&tag[2..]));
    tag_ok && !digits.is_empty() && hex::is_lower_hex(digits)
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RefName({:?})", self.0)
    }
}

impl FromStr for RefName {
    type Err = BeeError;
    fn from_str(s: &str) -> Result<Self> {
        RefName::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ref_names_accepted() {
        for good in [
            "main",
            "release/1.2",
            "green/ci-2026",
            "ws/01HZX3/feature-x",
            "feature/foo+bar",
            "fix_123",
            "c1",
            "c1_xyz",
            "k_",
            "naïve",
            "a.b",
            "HEAD",
        ] {
            assert!(RefName::new(good).is_ok(), "{good}");
        }
        assert!(RefName::new("x".repeat(255)).is_ok());
    }

    #[test]
    fn ref_names_rejected() {
        for bad in [
            "",
            "a..b",
            "a//b",
            "/a",
            "a/",
            ".hidden",
            "a/.b",
            "a.",
            "a/b.lock",
            "with space",
            "tab\tname",
            "main@7",
            "main~1",
            "x^2",
            "git:abc",
            "a?b",
            "a*",
            "a[1]",
            "a\\b",
            "del\u{7f}",
            "c1_abcdef",
            "k_00ff",
            "d110_00",
            "t1_0",
        ] {
            let err = RefName::new(bad).unwrap_err();
            assert!(err.is_invalid(Subcode::InvalidRefName), "{bad}");
        }
        assert!(RefName::new("x".repeat(256)).is_err());
    }

    #[test]
    fn namespaces() {
        let ns = |s: &str| RefName::new(s).unwrap().namespace();
        assert_eq!(ns("main"), RefNamespace::Protected);
        assert_eq!(ns("release/2"), RefNamespace::Protected);
        assert_eq!(ns("green/x"), RefNamespace::Green);
        assert_eq!(ns("ws/p/n"), RefNamespace::Workspace);
        assert_eq!(ns("ws/p"), RefNamespace::Other);
        assert_eq!(ns("mainline"), RefNamespace::Other);
        let ws = RefName::new("ws/01ABC/work/x").unwrap();
        assert_eq!(ws.workspace_owner(), Some("01ABC"));
        assert_eq!(RefName::new("main").unwrap().workspace_owner(), None);
        assert_eq!(ws.as_str(), "ws/01ABC/work/x");
        assert_eq!(ws.to_string(), "ws/01ABC/work/x");
        assert_eq!(
            format!("{:?}", RefName::new("m").unwrap()),
            "RefName(\"m\")"
        );
        assert_eq!("main".parse::<RefName>().unwrap().as_str(), "main");
    }

    proptest! {
        #[test]
        fn simple_names_are_valid(parts in proptest::collection::vec("[a-z][a-z0-9-]{0,10}", 1..5)) {
            let name = parts.join("/");
            prop_assert!(RefName::new(name).is_ok());
        }

        #[test]
        fn arbitrary_strings_never_panic(s in ".{0,40}") {
            if let Ok(r) = RefName::new(s.clone()) {
                prop_assert!(!r.as_str().contains(".."));
                prop_assert!(!r.as_str().contains('@'));
            }
        }
    }
}
