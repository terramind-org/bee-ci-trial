//! The `git_mapping` derived rows (SPEC §8, §17.4): `git sha ↔ changeset id`, written by the
//! importer and read here to resolve `git:<sha>` revisions.
//!
//! Two rows per imported commit, type `git_mapping`, version [`GIT_MAPPING_VERSION`]:
//!
//! - key = the changeset id, value = the commit's raw git object id (20 bytes for SHA-1, 32
//!   for SHA-256);
//! - key = the foreign id of the commit (`g2_…`: algorithm `0x02`, kind `0x00`, the git id
//!   zero-padded to 32 bytes), value = the changeset id's 34 bytes.
//!
//! [`git_mapping_rows`] builds both. Full ids resolve with one row read; abbreviations scan
//! the repo's changesets (the store has no prefix index over derived rows).

use bee_core::{BeeError, ObjectId, ObjectKind, Reason, Result, Subcode, hex};
use bee_store::DerivedRow;

use crate::codec::corrupt;
use crate::repo::History;

/// The derived type name.
pub const GIT_MAPPING_TYPE: &str = "git_mapping";
/// The derived type version.
pub const GIT_MAPPING_VERSION: u32 = 1;

/// The two rows recording that `changeset` is the git commit `git_oid` (raw bytes).
pub fn git_mapping_rows(changeset: ObjectId, git_oid: &[u8]) -> Result<Vec<DerivedRow>> {
    changeset.expect_kind(ObjectKind::Changeset)?;
    let foreign = ObjectId::git_foreign(git_oid)?;
    Ok(vec![
        DerivedRow {
            key: changeset,
            type_name: GIT_MAPPING_TYPE.into(),
            version: GIT_MAPPING_VERSION,
            value: git_oid.to_vec(),
        },
        DerivedRow {
            key: foreign,
            type_name: GIT_MAPPING_TYPE.into(),
            version: GIT_MAPPING_VERSION,
            value: changeset.as_bytes().to_vec(),
        },
    ])
}

fn bad_git(text: &str) -> BeeError {
    BeeError::invalid(
        Subcode::InvalidRevision,
        format!("git id must be 7 to 64 lowercase hex digits: {text:?}"),
    )
}

impl History {
    /// The git commit id an imported changeset came from, as raw bytes.
    pub async fn git_oid_of(&self, changeset: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.derived(changeset, GIT_MAPPING_TYPE, GIT_MAPPING_VERSION)
            .await
    }

    /// The changeset imported from a full git commit id (raw bytes).
    pub async fn changeset_for_git_oid(&self, git_oid: &[u8]) -> Result<Option<ObjectId>> {
        let key = ObjectId::git_foreign(git_oid)?;
        match self
            .derived(&key, GIT_MAPPING_TYPE, GIT_MAPPING_VERSION)
            .await?
        {
            Some(v) => ObjectId::from_bytes(&v)
                .map(Some)
                .map_err(|e| corrupt(GIT_MAPPING_TYPE, e)),
            None => Ok(None),
        }
    }

    /// Resolves `git:<hex>`: a full id (40 or 64 digits) by one row read, an abbreviation
    /// (7 or more digits) by scanning. `NOT_FOUND` when nothing matches; `AMBIGUOUS_PREFIX`
    /// with the sorted changeset ids under `candidates` when several do.
    pub async fn resolve_git(&self, text: &str) -> Result<ObjectId> {
        if text.len() < 7 || text.len() > 64 || !hex::is_lower_hex(text) {
            return Err(bad_git(text));
        }
        let not_found = || BeeError::new(Reason::NotFound, format!("no changeset for git:{text}"));
        if (text.len() == 40 || text.len() == 64)
            && let Some(bytes) = hex::decode(text)
        {
            return self
                .changeset_for_git_oid(&bytes)
                .await?
                .ok_or_else(not_found);
        }
        let mut found = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .meta()
                .list_changesets(self.repo(), after, 1000)
                .await?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for row in &page {
                if let Some(oid) = self.git_oid_of(&row.id).await?
                    && hex::encode(&oid).starts_with(text)
                {
                    found.push(row.id);
                }
            }
        }
        found.sort();
        match found.len() {
            0 => Err(not_found()),
            1 => Ok(found[0]),
            n => Err(BeeError::new(
                Reason::AmbiguousPrefix,
                format!("git:{text} matches {n} changesets"),
            )
            .with_payload(
                "candidates",
                bee_core::PayloadValue::List(found.iter().map(|id| id.to_text().into()).collect()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_object::Changeset;

    #[test]
    fn rows_cover_both_directions() {
        let cs = Changeset::empty_root().id();
        let rows = git_mapping_rows(cs, &[7; 20]).unwrap();
        assert_eq!(rows[0].key, cs);
        assert_eq!(rows[0].value, vec![7; 20]);
        assert_eq!(rows[1].key.kind(), ObjectKind::Foreign);
        assert_eq!(rows[1].value, cs.as_bytes().to_vec());
        assert!(git_mapping_rows(bee_object::blob_id(b""), &[7; 20]).is_err());
        assert!(git_mapping_rows(cs, &[7; 3]).is_err());
    }
}
