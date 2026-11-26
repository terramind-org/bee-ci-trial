//! The bidirectional index between git ids and bee ids, behind the derived `git_mapping`
//! (SPEC §8, §17.4).
//!
//! Commits map to changesets, trees to trees and blobs to blobs. The bee → git direction is
//! a function: one bee object has exactly one git representation, so mapping a bee id to
//! a second, different git id is an `INTEGRITY` error. The git → bee direction keeps the
//! first bee id recorded for a git id: two bee objects that differ only in data git does
//! not carry (an unknown tree extension, say) share a git id, and the first one wins.

use std::collections::BTreeMap;

use bee_core::{BeeError, ObjectId, ObjectKind, Result, Subcode};

use crate::error::integrity;
use crate::oid::GitOid;

/// A git id ↔ bee id index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitMapping {
    git_to_bee: BTreeMap<GitOid, ObjectId>,
    bee_to_git: BTreeMap<ObjectId, GitOid>,
}

impl GitMapping {
    /// An empty mapping.
    pub fn new() -> GitMapping {
        GitMapping::default()
    }

    /// Records that `bee` is represented in git as `git`. Only changesets, trees and blobs
    /// are mapped (`WRONG_KIND` otherwise). Fails with `INTEGRITY` when `bee` is already
    /// mapped to a different git id; re-recording the same pair is a no-op.
    pub fn insert(&mut self, git: GitOid, bee: ObjectId) -> Result<()> {
        if !matches!(
            bee.kind(),
            ObjectKind::Changeset | ObjectKind::Tree | ObjectKind::Blob
        ) {
            return Err(BeeError::invalid(
                Subcode::WrongKind,
                format!("only changesets, trees and blobs have git ids, not {bee}"),
            ));
        }
        if let Some(existing) = self.bee_to_git.get(&bee) {
            if *existing != git {
                return Err(integrity(format!(
                    "{bee} is already mapped to git {existing}, not {git}"
                ))
                .with_payload("oid", existing.to_hex()));
            }
            return Ok(());
        }
        if let Some(first) = self.git_to_bee.get(&git)
            && first.kind() != bee.kind()
        {
            return Err(integrity(format!(
                "git {git} already maps to {first}, which is a different kind than {bee}"
            )));
        }
        self.bee_to_git.insert(bee, git);
        self.git_to_bee.entry(git).or_insert(bee);
        Ok(())
    }

    /// The bee id for a git id.
    pub fn bee(&self, git: &GitOid) -> Option<ObjectId> {
        self.git_to_bee.get(git).copied()
    }

    /// The git id for a bee id.
    pub fn git(&self, bee: &ObjectId) -> Option<GitOid> {
        self.bee_to_git.get(bee).copied()
    }

    /// Number of bee ids mapped.
    pub fn len(&self) -> usize {
        self.bee_to_git.len()
    }

    /// True when nothing is mapped.
    pub fn is_empty(&self) -> bool {
        self.bee_to_git.is_empty()
    }

    /// Every `(bee id, git id)` pair, sorted by bee id.
    pub fn iter(&self) -> impl Iterator<Item = (&ObjectId, &GitOid)> {
        self.bee_to_git.iter()
    }

    /// Only the changeset ↔ commit pairs, sorted by changeset id.
    pub fn commits(&self) -> impl Iterator<Item = (&ObjectId, &GitOid)> {
        self.bee_to_git
            .iter()
            .filter(|(s, _)| s.kind() == ObjectKind::Changeset)
    }

    /// Adds every pair of `other`, with the same rules as [`GitMapping::insert`].
    pub fn extend(&mut self, other: &GitMapping) -> Result<()> {
        for (s, g) in other.iter() {
            self.insert(*g, *s)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::Reason;
    use bee_object::{Changeset, Meta, Tree, blob_id};

    #[test]
    fn both_directions() {
        let mut m = GitMapping::new();
        assert!(m.is_empty());
        let cs = Changeset::empty_root().id();
        let g = GitOid::Sha1([1; 20]);
        m.insert(g, cs).unwrap();
        m.insert(g, cs).unwrap();
        assert_eq!(m.bee(&g), Some(cs));
        assert_eq!(m.git(&cs), Some(g));
        assert_eq!(m.len(), 1);
        assert_eq!(m.commits().count(), 1);
        let err = m.insert(GitOid::Sha1([2; 20]), cs).unwrap_err();
        assert_eq!(err.reason, Reason::Integrity);
        // A git id already holding a changeset cannot also name a tree.
        assert!(m.insert(g, Tree::empty().id()).is_err());
        assert!(
            m.insert(GitOid::Sha1([3; 20]), Meta::empty_root().id())
                .unwrap_err()
                .is_invalid(Subcode::WrongKind)
        );
        // Two blobs cannot share a git id in practice, but the first recorded one wins.
        let b1 = blob_id(b"1");
        let b2 = blob_id(b"2");
        let gb = GitOid::Sha1([4; 20]);
        m.insert(gb, b1).unwrap();
        m.insert(gb, b2).unwrap();
        assert_eq!(m.bee(&gb), Some(b1));
        assert_eq!(m.git(&b2), Some(gb));
        let mut other = GitMapping::new();
        other.extend(&m).unwrap();
        assert_eq!(other.len(), m.len());
        assert_eq!(other.git(&b2), Some(gb));
        assert_eq!(other.iter().count(), 3);
    }
}
