//! Revision expressions (SPEC §5, "Revision expressions"): parsing into an AST.
//! Resolution against refs and the changeset graph happens in the server.
//!
//! ```text
//! revision := base suffix*
//! base     := "git:" 7..64 lowercase hex      git commit id or abbreviation
//!           | "k_" 32 lowercase hex           ChangeId
//!           | "c1_" 64 lowercase hex          changeset id
//!           | "c1_" 10..63 lowercase hex      changeset id prefix
//!           | refname "@" version             ref at a version (decimal, >= 1)
//!           | refname
//! suffix   := "~" [n]                         n-th first-parent ancestor (default 1)
//!           | "^" [n]                         n-th parent, 1-based (default 1; ^0 = itself)
//! ```
//! Numbers are decimal without leading zeros. The whole expression is at most 4096 bytes.

use std::fmt;
use std::str::FromStr;

use crate::change_id::ChangeId;
use crate::error::{BeeError, Result, Subcode};
use crate::hex;
use crate::id::{DIGEST_LEN, ObjectId, ObjectIdPrefix, ObjectKind};
use crate::refname::{RefName, is_id_shaped};

/// Max length of a revision expression in bytes.
pub const MAX_REVISION_BYTES: usize = 4096;
/// Min hex digits in a `git:` abbreviation.
pub const MIN_GIT_HEX: usize = 7;

/// A parsed revision expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// What the expression starts from.
    pub base: RevisionBase,
    /// Navigation steps applied left to right.
    pub suffixes: Vec<RevisionSuffix>,
}

/// The starting point of a revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevisionBase {
    /// A ref's current target.
    Ref(RefName),
    /// A ref's target at a given version (`name@version`).
    RefAt(RefName, u64),
    /// A full changeset id.
    Changeset(ObjectId),
    /// An abbreviated changeset id.
    ChangesetPrefix(ObjectIdPrefix),
    /// A git commit id or abbreviation, lowercase hex (`git:<sha>`).
    Git(String),
    /// The changeset carrying a ChangeId on the ref being queried.
    Change(ChangeId),
}

/// One navigation step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionSuffix {
    /// `~n`: the n-th first-parent ancestor.
    Ancestor(u32),
    /// `^n`: the n-th parent (1-based); `^0` is the changeset itself.
    Parent(u32),
}

fn bad(text: &str, why: &str) -> BeeError {
    BeeError::invalid(
        Subcode::InvalidRevision,
        format!("invalid revision {text:?}: {why}"),
    )
}

fn parse_number(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

impl Revision {
    /// Parses a revision expression.
    pub fn parse(text: &str) -> Result<Revision> {
        if text.is_empty() {
            return Err(bad(text, "empty"));
        }
        if text.len() > MAX_REVISION_BYTES {
            return Err(bad(text, "longer than 4096 bytes"));
        }
        let split = text.find(['~', '^']).unwrap_or(text.len());
        let (base_text, mut rest) = text.split_at(split);
        let base = parse_base(text, base_text)?;
        let mut suffixes = Vec::new();
        while let Some(op) = rest.chars().next() {
            rest = &rest[1..];
            let end = rest.find(['~', '^']).unwrap_or(rest.len());
            let (digits, tail) = rest.split_at(end);
            rest = tail;
            let n = if digits.is_empty() {
                1
            } else {
                let n = parse_number(digits).ok_or_else(|| bad(text, "bad number"))?;
                u32::try_from(n).map_err(|_| bad(text, "number too large"))?
            };
            suffixes.push(if op == '~' {
                RevisionSuffix::Ancestor(n)
            } else {
                RevisionSuffix::Parent(n)
            });
        }
        Ok(Revision { base, suffixes })
    }
}

fn parse_base(text: &str, base: &str) -> Result<RevisionBase> {
    if base.is_empty() {
        return Err(bad(text, "missing base"));
    }
    if let Some(sha) = base.strip_prefix("git:") {
        if sha.len() < MIN_GIT_HEX || sha.len() > 64 || !hex::is_lower_hex(sha) {
            return Err(bad(text, "git id must be 7 to 64 lowercase hex digits"));
        }
        return Ok(RevisionBase::Git(sha.to_owned()));
    }
    if base.starts_with("k_") {
        return ChangeId::parse(base)
            .map(RevisionBase::Change)
            .map_err(|_| bad(text, "change id must be k_ and 32 lowercase hex digits"));
    }
    if is_id_shaped(base) {
        let digits = base.split_once('_').map_or("", |(_, d)| d);
        if !base.starts_with("c1_") {
            return Err(bad(text, "only changeset ids name revisions"));
        }
        if digits.len() == DIGEST_LEN * 2 {
            let id = ObjectId::parse(base).map_err(|_| bad(text, "bad changeset id"))?;
            return Ok(RevisionBase::Changeset(id));
        }
        let prefix = ObjectIdPrefix::parse(base)
            .map_err(|_| bad(text, "changeset prefix needs 10 to 64 hex digits"))?;
        debug_assert_eq!(prefix.kind(), ObjectKind::Changeset);
        return Ok(RevisionBase::ChangesetPrefix(prefix));
    }
    if let Some((name, version)) = base.split_once('@') {
        let name = RefName::new(name).map_err(|e| bad(text, &e.message))?;
        let v = parse_number(version).ok_or_else(|| bad(text, "bad version"))?;
        if v == 0 {
            return Err(bad(text, "versions start at 1"));
        }
        return Ok(RevisionBase::RefAt(name, v));
    }
    RefName::new(base)
        .map(RevisionBase::Ref)
        .map_err(|e| bad(text, &e.message))
}

impl fmt::Display for RevisionBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RevisionBase::Ref(r) => write!(f, "{r}"),
            RevisionBase::RefAt(r, v) => write!(f, "{r}@{v}"),
            RevisionBase::Changeset(id) => write!(f, "{id}"),
            RevisionBase::ChangesetPrefix(p) => write!(f, "{p}"),
            RevisionBase::Git(sha) => write!(f, "git:{sha}"),
            RevisionBase::Change(c) => write!(f, "{c}"),
        }
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.base)?;
        for s in &self.suffixes {
            match s {
                RevisionSuffix::Ancestor(n) => write!(f, "~{n}")?,
                RevisionSuffix::Parent(n) => write!(f, "^{n}")?,
            }
        }
        Ok(())
    }
}

impl FromStr for Revision {
    type Err = BeeError;
    fn from_str(s: &str) -> Result<Self> {
        Revision::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rev(s: &str) -> Revision {
        Revision::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn revision_bases() {
        assert_eq!(
            rev("main").base,
            RevisionBase::Ref(RefName::new("main").unwrap())
        );
        assert_eq!(
            rev("main@7").base,
            RevisionBase::RefAt(RefName::new("main").unwrap(), 7)
        );
        assert_eq!(
            rev("release/1.0@12").base,
            RevisionBase::RefAt(RefName::new("release/1.0").unwrap(), 12)
        );
        let id = format!("c1_{}", "ab".repeat(32));
        assert_eq!(
            rev(&id).base,
            RevisionBase::Changeset(ObjectId::parse(&id).unwrap())
        );
        match rev("c1_0123456789").base {
            RevisionBase::ChangesetPrefix(p) => assert_eq!(p.hex(), "0123456789"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            rev(&format!("git:{}", "a".repeat(40))).base,
            RevisionBase::Git("a".repeat(40))
        );
        assert_eq!(rev("git:abc1234").base, RevisionBase::Git("abc1234".into()));
        assert_eq!(
            rev(&format!("git:{}", "b".repeat(64))).base,
            RevisionBase::Git("b".repeat(64))
        );
        let k = format!("k_{}", "0f".repeat(16));
        assert_eq!(
            rev(&k).base,
            RevisionBase::Change(ChangeId::parse(&k).unwrap())
        );
        assert!(rev("main").suffixes.is_empty());
    }

    #[test]
    fn revision_suffixes() {
        assert_eq!(rev("main~3").suffixes, [RevisionSuffix::Ancestor(3)]);
        assert_eq!(rev("main~").suffixes, [RevisionSuffix::Ancestor(1)]);
        assert_eq!(rev("X^2").suffixes, [RevisionSuffix::Parent(2)]);
        assert_eq!(rev("X^").suffixes, [RevisionSuffix::Parent(1)]);
        assert_eq!(rev("X^0").suffixes, [RevisionSuffix::Parent(0)]);
        assert_eq!(
            rev("main@7~2^2~").suffixes,
            [
                RevisionSuffix::Ancestor(2),
                RevisionSuffix::Parent(2),
                RevisionSuffix::Ancestor(1)
            ]
        );
        assert_eq!(
            rev("c1_0123456789~1").suffixes,
            [RevisionSuffix::Ancestor(1)]
        );
        assert_eq!(
            rev("main~4294967295").suffixes,
            [RevisionSuffix::Ancestor(u32::MAX)]
        );
    }

    #[test]
    fn revision_rejections() {
        let long_git = format!("git:{}", "a".repeat(65));
        let long_id = format!("c1_{}", "0".repeat(65));
        let tree_id = format!("t1_{}", "0".repeat(64));
        let too_long = "a".repeat(4097);
        for bad in [
            "",
            "~1",
            "^",
            "main~01",
            "main~4294967296",
            "main~x",
            "main@0",
            "main@",
            "main@01",
            "main@x",
            "@7",
            "a..b",
            "git:",
            "git:abc12",
            "git:ABCDEF0",
            &long_git,
            "k_1234",
            "c1_012345678",
            &long_id,
            &tree_id,
            "b1_0123456789",
            "main@7@8",
            &too_long,
        ] {
            let err = Revision::parse(bad).unwrap_err();
            assert!(err.is_invalid(Subcode::InvalidRevision), "{bad}: {err}");
        }
    }

    #[test]
    fn revision_display_is_canonical() {
        assert_eq!(rev("main~").to_string(), "main~1");
        assert_eq!(rev("X^").to_string(), "X^1");
        assert_eq!(rev("main@7~2^2").to_string(), "main@7~2^2");
        assert_eq!(rev("git:abc1234").to_string(), "git:abc1234");
        assert_eq!(rev("c1_0123456789").to_string(), "c1_0123456789");
        let k = format!("k_{}", "0f".repeat(16));
        assert_eq!(rev(&k).to_string(), k);
        let id = format!("c1_{}", "ab".repeat(32));
        assert_eq!(rev(&id).to_string(), id);
        assert_eq!("main".parse::<Revision>().unwrap(), rev("main"));
    }

    proptest! {
        #[test]
        fn display_round_trips(
            name in "[a-z][a-z0-9-]{0,8}(/[a-z0-9-]{1,8}){0,2}",
            version in proptest::option::of(1u64..1000),
            steps in proptest::collection::vec((any::<bool>(), 0u32..50), 0..5),
        ) {
            let mut text = name.clone();
            if let Some(v) = version {
                text.push_str(&format!("@{v}"));
            }
            for (anc, n) in &steps {
                text.push(if *anc { '~' } else { '^' });
                text.push_str(&n.to_string());
            }
            let r = Revision::parse(&text).unwrap();
            prop_assert_eq!(r.suffixes.len(), steps.len());
            prop_assert_eq!(r.to_string(), text.clone());
            prop_assert_eq!(Revision::parse(&r.to_string()).unwrap(), r);
        }

        #[test]
        fn arbitrary_text_never_panics(s in ".{0,30}") {
            let _ = Revision::parse(&s);
        }
    }
}
