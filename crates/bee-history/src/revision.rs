//! Revision resolution (SPEC §5): the syntax tree parsed by `bee-core` resolved against
//! refs, the ref log, the landed index, the changeset table, `git_mapping` and the graph.

use bee_core::{
    BeeError, ObjectId, ObjectIdPrefix, PayloadValue, Reason, RefName, Result, Revision,
    RevisionBase, RevisionSuffix,
};

use crate::graph::no_such_ancestor;
use crate::repo::{History, changeset_not_found};

/// How many prefix matches are listed in an `AMBIGUOUS_PREFIX` payload.
const MAX_CANDIDATES: usize = 16;

fn ref_not_found(name: &RefName) -> BeeError {
    BeeError::new(Reason::NotFound, format!("ref {name} not found"))
        .with_payload("ref", name.as_str())
}

impl History {
    /// Parses and resolves a revision expression. `on_ref` is the ref a `k_…` ChangeId is
    /// looked up on (its landed changeset there); without it the ChangeId must name exactly
    /// one changeset of the repo.
    pub async fn resolve_str(&self, text: &str, on_ref: Option<&RefName>) -> Result<ObjectId> {
        self.resolve(&Revision::parse(text)?, on_ref).await
    }

    /// Resolves a parsed revision to a changeset id.
    ///
    /// Errors: `NOT_FOUND` for a missing ref, version, changeset, ChangeId, git id or
    /// ancestor; `AMBIGUOUS_PREFIX` (sorted matches under `candidates`) for an abbreviation
    /// or ChangeId matching several changesets.
    pub async fn resolve(&self, rev: &Revision, on_ref: Option<&RefName>) -> Result<ObjectId> {
        let mut id = self.resolve_base(&rev.base, on_ref).await?;
        for s in &rev.suffixes {
            id = match *s {
                RevisionSuffix::Ancestor(n) => self
                    .first_parent_ancestor(&id, u64::from(n))
                    .await?
                    .ok_or_else(|| no_such_ancestor(&format!("{id}~{n}")))?,
                RevisionSuffix::Parent(n) => self
                    .nth_parent(&id, n)
                    .await?
                    .ok_or_else(|| no_such_ancestor(&format!("{id}^{n}")))?,
            };
        }
        Ok(id)
    }

    async fn resolve_base(
        &self,
        base: &RevisionBase,
        on_ref: Option<&RefName>,
    ) -> Result<ObjectId> {
        match base {
            RevisionBase::Ref(name) => self
                .meta()
                .get_ref(self.repo(), name)
                .await?
                .and_then(|r| r.target)
                .ok_or_else(|| ref_not_found(name)),
            RevisionBase::RefAt(name, version) => {
                let log = self.meta().ref_log(self.repo(), name, *version, 1).await?;
                log.into_iter()
                    .find(|e| e.version == *version)
                    .and_then(|e| e.new)
                    .ok_or_else(|| {
                        BeeError::new(Reason::NotFound, format!("{name}@{version} has no target"))
                            .with_payload("ref", name.as_str())
                    })
            }
            RevisionBase::Changeset(id) => {
                if self.has_changeset(id).await? {
                    Ok(*id)
                } else {
                    Err(changeset_not_found(id))
                }
            }
            RevisionBase::ChangesetPrefix(p) => self.resolve_prefix(p).await,
            RevisionBase::Git(hex) => self.resolve_git(hex).await,
            RevisionBase::Change(change_id) => {
                if let Some(name) = on_ref {
                    let landed = self
                        .meta()
                        .get_landed(self.repo(), name, std::slice::from_ref(change_id))
                        .await?;
                    if let Some(Some(row)) = landed.into_iter().next() {
                        return Ok(row.result);
                    }
                    return Err(BeeError::new(
                        Reason::NotFound,
                        format!("{change_id} has not landed on {name}"),
                    ));
                }
                let ids = self
                    .meta()
                    .changesets_by_change_id(self.repo(), change_id)
                    .await?;
                match ids.len() {
                    0 => Err(BeeError::new(
                        Reason::NotFound,
                        format!("no changeset carries {change_id}"),
                    )),
                    1 => Ok(ids[0]),
                    n => Err(BeeError::new(
                        Reason::AmbiguousPrefix,
                        format!("{change_id} names {n} changesets; give a ref"),
                    )
                    .with_payload(
                        "candidates",
                        PayloadValue::List(ids.iter().map(|i| i.to_text().into()).collect()),
                    )),
                }
            }
        }
    }

    /// Resolves an abbreviated changeset id among this repo's changesets.
    pub async fn resolve_prefix(&self, prefix: &ObjectIdPrefix) -> Result<ObjectId> {
        let ids = self
            .meta()
            .changesets_with_prefix(self.repo(), prefix, MAX_CANDIDATES)
            .await?;
        prefix.resolve(ids.iter())
    }
}
