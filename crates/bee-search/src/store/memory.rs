//! The in-memory [`IndexStore`].

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use bee_core::{ObjectId, Result};
use bee_store::{OrgId, RepoId};

use super::{DocNum, Document, IndexStore, Manifest};
use crate::trigram::Trigram;

#[derive(Default)]
struct OrgIndex {
    by_blob: BTreeMap<ObjectId, DocNum>,
    docs: BTreeMap<DocNum, Arc<Document>>,
    postings: BTreeMap<Trigram, Vec<DocNum>>,
    next: DocNum,
}

#[derive(Default)]
struct State {
    orgs: BTreeMap<OrgId, OrgIndex>,
    manifests: BTreeMap<(OrgId, RepoId, String), Arc<Manifest>>,
}

/// An [`IndexStore`] held entirely in memory.
#[derive(Default)]
pub struct MemoryIndexStore {
    state: RwLock<State>,
}

impl MemoryIndexStore {
    /// An empty store.
    pub fn new() -> MemoryIndexStore {
        MemoryIndexStore::default()
    }

    fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn write<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.write().unwrap_or_else(PoisonError::into_inner))
    }
}

impl IndexStore for MemoryIndexStore {
    fn lookup(&self, org: OrgId, blobs: &[ObjectId]) -> Result<Vec<Option<DocNum>>> {
        Ok(self.read(|s| {
            let o = s.orgs.get(&org);
            blobs
                .iter()
                .map(|b| o.and_then(|o| o.by_blob.get(b).copied()))
                .collect()
        }))
    }

    fn add_documents(&self, org: OrgId, docs: Vec<Document>) -> Result<Vec<DocNum>> {
        Ok(self.write(|s| {
            let o = s.orgs.entry(org).or_default();
            docs.into_iter()
                .map(|d| {
                    if let Some(n) = o.by_blob.get(&d.blob) {
                        return *n;
                    }
                    let n = o.next;
                    o.next += 1;
                    for t in d.trigrams() {
                        o.postings.entry(t).or_default().push(n);
                    }
                    o.by_blob.insert(d.blob, n);
                    o.docs.insert(n, Arc::new(d));
                    n
                })
                .collect()
        }))
    }

    fn document(&self, org: OrgId, doc: DocNum) -> Result<Option<Arc<Document>>> {
        Ok(self.read(|s| s.orgs.get(&org).and_then(|o| o.docs.get(&doc).cloned())))
    }

    fn postings(&self, org: OrgId, trigram: Trigram) -> Result<Vec<DocNum>> {
        Ok(self.read(|s| {
            s.orgs
                .get(&org)
                .and_then(|o| o.postings.get(&trigram).cloned())
                .unwrap_or_default()
        }))
    }

    fn doc_count(&self, org: OrgId) -> Result<u64> {
        Ok(self.read(|s| s.orgs.get(&org).map_or(0, |o| o.docs.len() as u64)))
    }

    fn manifest(&self, org: OrgId, repo: RepoId, ref_name: &str) -> Result<Option<Arc<Manifest>>> {
        Ok(self.read(|s| s.manifests.get(&(org, repo, ref_name.to_string())).cloned()))
    }

    fn put_manifest(&self, manifest: Manifest) -> Result<()> {
        self.write(|s| {
            s.manifests.insert(
                (manifest.org, manifest.repo, manifest.ref_name.clone()),
                Arc::new(manifest),
            );
        });
        Ok(())
    }

    fn delete_manifest(&self, org: OrgId, repo: RepoId, ref_name: &str) -> Result<bool> {
        Ok(self.write(|s| {
            s.manifests
                .remove(&(org, repo, ref_name.to_string()))
                .is_some()
        }))
    }

    fn list_manifests(&self, org: OrgId) -> Result<Vec<(RepoId, String)>> {
        Ok(self.read(|s| {
            s.manifests
                .keys()
                .filter(|(o, _, _)| *o == org)
                .map(|(_, r, n)| (*r, n.clone()))
                .collect()
        }))
    }

    fn compact(&self, org: OrgId, keep: &dyn Fn(&ObjectId) -> bool) -> Result<usize> {
        Ok(self.write(|s| {
            let Some(o) = s.orgs.get_mut(&org) else {
                return 0;
            };
            let dropped: Vec<(ObjectId, DocNum)> = o
                .by_blob
                .iter()
                .filter(|(b, _)| !keep(b))
                .map(|(b, n)| (*b, *n))
                .collect();
            for (b, n) in &dropped {
                o.by_blob.remove(b);
                o.docs.remove(n);
            }
            let gone: std::collections::BTreeSet<DocNum> =
                dropped.iter().map(|(_, n)| *n).collect();
            o.postings.retain(|_, list| {
                list.retain(|n| !gone.contains(n));
                !list.is_empty()
            });
            dropped.len()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract() {
        super::super::contract::run(&MemoryIndexStore::new());
    }
}
