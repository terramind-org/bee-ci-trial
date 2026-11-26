//! Reference refs: versioned compare-and-swap, the ref log, the landed index and land
//! ordering (SPEC §5, §6.2 step 3, §7.4).

use std::collections::BTreeMap;

use bee_core::{ChangeId, ObjectId, RefName};
use bee_store::{Expected, LandedEntry};

/// One model ref-log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelLogEntry {
    /// Version produced.
    pub version: u64,
    /// Target before.
    pub old: Option<ObjectId>,
    /// Target after.
    pub new: Option<ObjectId>,
    /// Changes landed.
    pub landed: Vec<LandedEntry>,
}

/// Why the model refused an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefModelError {
    /// The precondition failed; the ref is at `current` (0 when it never existed).
    VersionMismatch {
        /// Current version.
        current: u64,
    },
    /// The ChangeId already landed on the ref (or twice in one update).
    AlreadyLanded {
        /// The change.
        change_id: ChangeId,
    },
    /// An update with no steps, or a deletion that lands changes.
    Invalid,
}

/// A step: new target and landed entries.
pub type ModelStep = (Option<ObjectId>, Vec<LandedEntry>);

/// The model of one repo's refs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefModel {
    refs: BTreeMap<RefName, (Option<ObjectId>, u64)>,
    log: BTreeMap<RefName, Vec<ModelLogEntry>>,
    landed: BTreeMap<(RefName, ChangeId), (ObjectId, ObjectId, u64)>,
}

/// What a model land did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandOutcome {
    /// Every member had already landed from the same source; nothing was written.
    AlreadyRecorded {
        /// Each member's recorded result, in stack order.
        results: Vec<ObjectId>,
    },
    /// The ref moved to a new version.
    Landed {
        /// The new version.
        version: u64,
        /// Each member's result (recorded or new), in stack order.
        results: Vec<ObjectId>,
    },
}

impl RefModel {
    /// No refs.
    pub fn new() -> RefModel {
        RefModel::default()
    }

    /// `(target, version)` of a ref.
    pub fn get(&self, name: &RefName) -> Option<(Option<ObjectId>, u64)> {
        self.refs.get(name).copied()
    }

    /// Every ref, sorted by name.
    pub fn refs(&self) -> Vec<(RefName, Option<ObjectId>, u64)> {
        self.refs
            .iter()
            .map(|(n, (t, v))| (n.clone(), *t, *v))
            .collect()
    }

    /// The ref log of a ref.
    pub fn log(&self, name: &RefName) -> &[ModelLogEntry] {
        self.log.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The landed index entry `(source, result, version)`.
    pub fn landed(
        &self,
        name: &RefName,
        change_id: &ChangeId,
    ) -> Option<(ObjectId, ObjectId, u64)> {
        self.landed.get(&(name.clone(), *change_id)).copied()
    }

    /// The whole landed index, sorted.
    pub fn landed_all(&self) -> Vec<(RefName, ChangeId, ObjectId, ObjectId, u64)> {
        self.landed
            .iter()
            .map(|((n, c), (s, r, v))| (n.clone(), *c, *s, *r, *v))
            .collect()
    }

    /// A compare-and-swap writing one version per step, all or nothing.
    pub fn update(
        &mut self,
        name: &RefName,
        expected: Expected,
        steps: &[ModelStep],
    ) -> Result<(u64, u64), RefModelError> {
        if steps.is_empty() {
            return Err(RefModelError::Invalid);
        }
        let (target, version) = self.refs.get(name).copied().unwrap_or((None, 0));
        let exists = self.refs.contains_key(name);
        let ok = match expected {
            Expected::Absent => target.is_none(),
            Expected::Version(v) => exists && v == version,
        };
        if !ok {
            return Err(RefModelError::VersionMismatch { current: version });
        }
        let mut seen = Vec::new();
        for (t, landed) in steps {
            if t.is_none() && !landed.is_empty() {
                return Err(RefModelError::Invalid);
            }
            for l in landed {
                if self.landed.contains_key(&(name.clone(), l.change_id))
                    || seen.contains(&l.change_id)
                {
                    return Err(RefModelError::AlreadyLanded {
                        change_id: l.change_id,
                    });
                }
                seen.push(l.change_id);
            }
        }
        let mut v = version;
        let mut old = target;
        for (t, landed) in steps {
            v += 1;
            self.log
                .entry(name.clone())
                .or_default()
                .push(ModelLogEntry {
                    version: v,
                    old,
                    new: *t,
                    landed: landed.clone(),
                });
            for l in landed {
                self.landed
                    .insert((name.clone(), l.change_id), (l.source, l.result, v));
            }
            old = *t;
        }
        self.refs.insert(name.clone(), (old, v));
        Ok((version, v))
    }

    /// Lands a linear stack (SPEC §7.4 steps 3–5): a member already landed from the same
    /// source is skipped; from a different source is `AlreadyLanded`; otherwise it is
    /// "rebased" onto the running tip by `rebase(tip, source)`. If every member was skipped
    /// the recorded results are returned and nothing is written; otherwise one CAS from the
    /// version read at the start writes one ref-log entry landing the new members.
    pub fn land(
        &mut self,
        name: &RefName,
        stack: &[(ChangeId, ObjectId)],
        rebase: &mut dyn FnMut(Option<ObjectId>, ObjectId) -> ObjectId,
    ) -> Result<LandOutcome, RefModelError> {
        let (tip, version) = self.refs.get(name).copied().unwrap_or((None, 0));
        let mut running = tip;
        let mut results = Vec::new();
        let mut entries = Vec::new();
        for (cid, source) in stack {
            match self.landed.get(&(name.clone(), *cid)) {
                Some((s, r, _)) if s == source => results.push(*r),
                Some(_) => return Err(RefModelError::AlreadyLanded { change_id: *cid }),
                None => {
                    let r = rebase(running, *source);
                    running = Some(r);
                    results.push(r);
                    entries.push(LandedEntry {
                        change_id: *cid,
                        source: *source,
                        result: r,
                    });
                }
            }
        }
        if entries.is_empty() {
            return Ok(LandOutcome::AlreadyRecorded { results });
        }
        let expected = if tip.is_none() {
            Expected::Absent
        } else {
            Expected::Version(version)
        };
        let (_, v) = self.update(name, expected, &[(running, entries)])?;
        Ok(LandOutcome::Landed {
            version: v,
            results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;

    fn id(n: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Changeset, [n; 32])
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::new([n; 16])
    }

    #[test]
    fn cas_versions_and_landed_index() {
        let main = RefName::new("main").unwrap();
        let mut m = RefModel::new();
        assert_eq!(
            m.update(&main, Expected::Absent, &[(Some(id(1)), vec![])]),
            Ok((0, 1))
        );
        assert_eq!(
            m.update(&main, Expected::Version(0), &[(Some(id(2)), vec![])]),
            Err(RefModelError::VersionMismatch { current: 1 })
        );
        let l = LandedEntry {
            change_id: cid(1),
            source: id(2),
            result: id(3),
        };
        assert_eq!(
            m.update(
                &main,
                Expected::Version(1),
                &[(Some(id(3)), vec![l]), (Some(id(4)), vec![])]
            ),
            Ok((1, 3))
        );
        assert_eq!(m.log(&main).len(), 3);
        assert_eq!(m.landed(&main, &cid(1)), Some((id(2), id(3), 2)));
        // A landed violation aborts everything.
        let again = m.update(&main, Expected::Version(3), &[(Some(id(5)), vec![l])]);
        assert_eq!(
            again,
            Err(RefModelError::AlreadyLanded { change_id: cid(1) })
        );
        assert_eq!(m.get(&main), Some((Some(id(4)), 3)));
        // Delete keeps numbering; Absent then recreates.
        assert_eq!(
            m.update(&main, Expected::Version(3), &[(None, vec![])]),
            Ok((3, 4))
        );
        assert_eq!(
            m.update(&main, Expected::Absent, &[(Some(id(1)), vec![])]),
            Ok((4, 5))
        );
        assert_eq!(
            m.update(&main, Expected::Version(5), &[]),
            Err(RefModelError::Invalid)
        );
        assert_eq!(m.refs().len(), 1);
        assert_eq!(m.landed_all().len(), 1);
    }

    #[test]
    fn land_is_idempotent_after_response_lost() {
        let main = RefName::new("main").unwrap();
        let mut m = RefModel::new();
        m.update(&main, Expected::Absent, &[(Some(id(1)), vec![])])
            .unwrap();
        let mut next = 100u8;
        let mut rebase = |_tip: Option<ObjectId>, _src: ObjectId| {
            next += 1;
            id(next)
        };
        let stack = [(cid(1), id(10)), (cid(2), id(11))];
        let first = m.land(&main, &stack, &mut rebase).unwrap();
        assert_eq!(
            first,
            LandOutcome::Landed {
                version: 2,
                results: vec![id(101), id(102)]
            }
        );
        let retry = m.land(&main, &stack, &mut rebase).unwrap();
        assert_eq!(
            retry,
            LandOutcome::AlreadyRecorded {
                results: vec![id(101), id(102)]
            }
        );
        let other = m.land(&main, &[(cid(1), id(12))], &mut rebase);
        assert_eq!(
            other,
            Err(RefModelError::AlreadyLanded { change_id: cid(1) })
        );
    }
}
