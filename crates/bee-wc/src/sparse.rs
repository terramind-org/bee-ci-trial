//! Sparse profiles (SPEC §10.3): a list of included path prefixes; empty means the whole repo.

use serde::{Deserialize, Serialize};

use crate::error::{Result, WcError};
use crate::fsutil::path_text;

/// Included path prefixes, normalized: sorted, unique, none inside another.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sparse {
    prefixes: Vec<String>,
}

fn under(path: &[u8], prefix: &[u8]) -> bool {
    path == prefix
        || (path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/')
}

impl Sparse {
    /// The whole repo.
    pub fn full() -> Self {
        Self::default()
    }

    /// A profile from prefixes given by a user: trailing and leading `/` are dropped, `.` or
    /// an empty prefix means the whole repo, and every prefix must be a valid repo path.
    pub fn from_prefixes<I, S>(prefixes: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Vec::new();
        for raw in prefixes {
            let p = raw.as_ref().trim_matches('/');
            if p.is_empty() || p == "." {
                return Ok(Self::full());
            }
            bee_core::RepoPath::new(p.as_bytes().to_vec()).map_err(|e| {
                WcError::usage(
                    "invalid_path",
                    format!("sparse prefix {p:?}: {}", e.message),
                )
            })?;
            out.push(p.to_owned());
        }
        Ok(Self::normalized(out))
    }

    fn normalized(mut prefixes: Vec<String>) -> Self {
        prefixes.sort();
        prefixes.dedup();
        let mut kept: Vec<String> = Vec::new();
        for p in prefixes {
            if !kept.iter().any(|k| under(p.as_bytes(), k.as_bytes())) {
                kept.push(p);
            }
        }
        Self { prefixes: kept }
    }

    /// True for the whole repo.
    pub fn is_full(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// The prefixes, sorted.
    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    /// True if `path` is inside the profile.
    pub fn contains(&self, path: &[u8]) -> bool {
        self.is_full() || self.prefixes.iter().any(|p| under(path, p.as_bytes()))
    }

    /// True if directory `dir` is inside the profile or holds a prefix.
    pub fn may_contain(&self, dir: &[u8]) -> bool {
        self.contains(dir) || self.prefixes.iter().any(|p| under(p.as_bytes(), dir))
    }

    /// This profile plus `more` (adding to a full profile keeps it full).
    pub fn with_added(&self, more: &Sparse) -> Sparse {
        if self.is_full() || more.is_full() {
            return Sparse::full();
        }
        let mut all = self.prefixes.clone();
        all.extend(more.prefixes.iter().cloned());
        Self::normalized(all)
    }

    /// This profile without `less`. Removing from a full profile is refused, since the result
    /// cannot be written as a list of prefixes.
    pub fn with_removed(&self, less: &Sparse) -> Result<Sparse> {
        if self.is_full() {
            return Err(WcError::usage(
                "full_profile",
                "the profile is the whole repo; set prefixes with `bee sparse set` first",
            ));
        }
        let kept: Vec<String> = self
            .prefixes
            .iter()
            .filter(|p| {
                !less
                    .prefixes
                    .iter()
                    .any(|l| under(p.as_bytes(), l.as_bytes()))
            })
            .cloned()
            .collect();
        if kept.is_empty() {
            return Err(WcError::usage(
                "empty_profile",
                "removing these prefixes would leave nothing checked out",
            ));
        }
        Ok(Self::normalized(kept))
    }

    /// The prefixes as display strings (`.` for the whole repo).
    pub fn display(&self) -> Vec<String> {
        if self.is_full() {
            vec![".".to_owned()]
        } else {
            self.prefixes
                .iter()
                .map(|p| path_text(p.as_bytes()))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership() {
        let s = Sparse::from_prefixes(["services/api/", "docs", "services/api/v2"]).expect("ok");
        assert_eq!(
            s.prefixes(),
            &["docs".to_owned(), "services/api".to_owned()]
        );
        assert!(s.contains(b"services/api/main.rs"));
        assert!(s.contains(b"docs"));
        assert!(!s.contains(b"services/apix/a"));
        assert!(!s.contains(b"services/web/a"));
        assert!(s.may_contain(b"services"));
        assert!(!s.may_contain(b"web"));
        assert!(Sparse::from_prefixes(["."]).expect("full").is_full());
        assert!(Sparse::from_prefixes(["a//b"]).is_err());
    }

    #[test]
    fn add_and_remove() {
        let s = Sparse::from_prefixes(["a", "b"]).expect("ok");
        let added = s.with_added(&Sparse::from_prefixes(["c", "a/x"]).expect("ok"));
        assert_eq!(added.display(), vec!["a", "b", "c"]);
        let removed = added
            .with_removed(&Sparse::from_prefixes(["b"]).expect("ok"))
            .expect("rm");
        assert_eq!(removed.display(), vec!["a", "c"]);
        assert!(
            removed
                .with_removed(&Sparse::from_prefixes(["a", "c"]).expect("ok"))
                .is_err()
        );
        assert!(Sparse::full().with_removed(&removed).is_err());
        assert!(s.with_added(&Sparse::full()).is_full());
    }
}
