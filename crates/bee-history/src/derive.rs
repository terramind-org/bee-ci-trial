//! Forward derivation (SPEC §8): find the nearest ancestors that already have a stored value,
//! then derive forward in dependency order, writing each value back as it is produced.
//!
//! The walk is an explicit stack, never recursion, so a value missing for a million
//! ancestors costs memory proportional to the gap and no call depth. Values are written
//! insert-or-ignore, so two derivations racing on the same key are harmless.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason, Result};

/// A derived value computed from the values of its dependencies (for example a changeset's
/// parents, or a file-history node's previous versions).
#[async_trait]
pub trait Forward: Send + Sync {
    /// A short name for errors.
    fn what(&self) -> &str;

    /// The stored value for `key`, if any.
    async fn stored(&self, key: &ObjectId) -> Result<Option<Vec<u8>>>;

    /// The keys whose values [`Forward::derive`] needs for `key`, in a fixed order.
    async fn deps(&self, key: &ObjectId) -> Result<Vec<ObjectId>>;

    /// Derives `key`'s value from its dependencies' values (given in [`Forward::deps`] order).
    /// Must be deterministic.
    async fn derive(&self, key: &ObjectId, deps: &[(ObjectId, Vec<u8>)]) -> Result<Vec<u8>>;

    /// Writes a derived value back, insert-or-ignore.
    async fn store(&self, key: &ObjectId, value: &[u8]) -> Result<()>;
}

/// What [`ensure`] did, for tests and metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnsureStats {
    /// Values found stored.
    pub found: u64,
    /// Values derived (and written back).
    pub derived: u64,
}

/// Returns `key`'s value, deriving it and any missing dependencies forward from the nearest
/// stored values.
pub async fn ensure<F: Forward + ?Sized>(f: &F, key: &ObjectId) -> Result<Vec<u8>> {
    ensure_with_stats(f, key).await.map(|(v, _)| v)
}

/// [`ensure`], also reporting how many values were found and derived.
pub async fn ensure_with_stats<F: Forward + ?Sized>(
    f: &F,
    key: &ObjectId,
) -> Result<(Vec<u8>, EnsureStats)> {
    let mut stats = EnsureStats::default();
    if let Some(v) = f.stored(key).await? {
        stats.found += 1;
        return Ok((v, stats));
    }
    let mut values: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
    let mut in_progress: BTreeSet<ObjectId> = BTreeSet::new();
    let mut stack: Vec<(ObjectId, Option<Vec<ObjectId>>)> = vec![(*key, None)];
    while let Some(top) = stack.last_mut() {
        let k = top.0;
        if values.contains_key(&k) && top.1.is_none() {
            stack.pop();
            continue;
        }
        match top.1.take() {
            None => {
                let deps = f.deps(&k).await?;
                in_progress.insert(k);
                let mut missing = Vec::new();
                for d in &deps {
                    if values.contains_key(d) {
                        continue;
                    }
                    if in_progress.contains(d) {
                        return Err(BeeError::new(
                            Reason::Internal,
                            format!("{}: dependency cycle through {d}", f.what()),
                        ));
                    }
                    match f.stored(d).await? {
                        Some(v) => {
                            stats.found += 1;
                            values.insert(*d, v);
                        }
                        None => missing.push(*d),
                    }
                }
                stack.last_mut().expect("top exists").1 = Some(deps);
                for d in missing.into_iter().rev() {
                    stack.push((d, None));
                }
            }
            Some(deps) => {
                let inputs: Vec<(ObjectId, Vec<u8>)> = deps
                    .iter()
                    .map(|d| {
                        values.get(d).map(|v| (*d, v.clone())).ok_or_else(|| {
                            BeeError::new(
                                Reason::Internal,
                                format!("{}: dependency {d} of {k} was not derived", f.what()),
                            )
                        })
                    })
                    .collect::<Result<_>>()?;
                let v = f.derive(&k, &inputs).await?;
                f.store(&k, &v).await?;
                stats.derived += 1;
                values.insert(k, v);
                in_progress.remove(&k);
                stack.pop();
            }
        }
    }
    let v = values.remove(key).ok_or_else(|| {
        BeeError::new(
            Reason::Internal,
            format!("{}: {key} was not derived", f.what()),
        )
    })?;
    Ok((v, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;
    use std::sync::Mutex;

    fn id(n: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Changeset, [n; 32])
    }

    /// Value = 1 + sum of dependency values; node n depends on n-1 (and n-2 when even).
    struct Chain {
        store: Mutex<BTreeMap<ObjectId, Vec<u8>>>,
        cyclic: bool,
    }

    #[async_trait]
    impl Forward for Chain {
        fn what(&self) -> &str {
            "chain"
        }
        async fn stored(&self, key: &ObjectId) -> Result<Option<Vec<u8>>> {
            Ok(self.store.lock().unwrap().get(key).cloned())
        }
        async fn deps(&self, key: &ObjectId) -> Result<Vec<ObjectId>> {
            let n = key.digest()[0];
            let mut d = Vec::new();
            if n > 0 {
                d.push(id(n - 1));
            }
            if n > 1 && n.is_multiple_of(2) {
                d.push(id(n - 2));
            }
            if self.cyclic && n == 0 {
                d.push(id(3));
            }
            Ok(d)
        }
        async fn derive(&self, _key: &ObjectId, deps: &[(ObjectId, Vec<u8>)]) -> Result<Vec<u8>> {
            let s = deps
                .iter()
                .map(|(_, v)| u64::from_be_bytes(v[..].try_into().unwrap()))
                .fold(1u64, u64::wrapping_add);
            Ok(s.to_be_bytes().to_vec())
        }
        async fn store(&self, key: &ObjectId, value: &[u8]) -> Result<()> {
            self.store
                .lock()
                .unwrap()
                .entry(*key)
                .or_insert(value.to_vec());
            Ok(())
        }
    }

    fn val(v: &[u8]) -> u64 {
        u64::from_be_bytes(v.try_into().unwrap())
    }

    #[tokio::test]
    async fn derives_forward_from_nearest_stored_values_iteratively() {
        let c = Chain {
            store: Mutex::new(BTreeMap::new()),
            cyclic: false,
        };
        let (v, s) = ensure_with_stats(&c, &id(200)).await.unwrap();
        assert_eq!(s.derived, 201);
        // Every value was written back; a second call derives nothing.
        let (v2, s2) = ensure_with_stats(&c, &id(200)).await.unwrap();
        assert_eq!((v, s2.derived, s2.found), (v2, 0, 1));
        // v0 = 1, v1 = 2, v2 = 1 + v1 + v0 = 4, v3 = 5, v4 = 1 + v3 + v2 = 10.
        assert_eq!(val(&ensure(&c, &id(4)).await.unwrap()), 10);
        // Removing a value re-derives only it, from the nearest stored dependencies.
        c.store.lock().unwrap().remove(&id(200));
        let (_, s3) = ensure_with_stats(&c, &id(200)).await.unwrap();
        assert_eq!(s3.derived, 1);
    }

    #[tokio::test]
    async fn cycles_are_internal_errors() {
        let c = Chain {
            store: Mutex::new(BTreeMap::new()),
            cyclic: true,
        };
        let e = ensure(&c, &id(5)).await.unwrap_err();
        assert_eq!(e.reason, Reason::Internal);
    }
}
