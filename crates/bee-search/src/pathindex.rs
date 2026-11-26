//! The path index (PRODUCT §4.4, docs/spec/search.md §3.3): a trigram index over the paths of
//! one manifest, so `path:` terms select candidate files without walking every path.
//!
//! Each path contributes its folded trigrams (the same [`Trigram`] the content index uses); a
//! `path:` pattern is planned into a [`TrigramQuery`] by the same planner as content
//! patterns, over the pattern's regex form. Planning only gives up selectivity, never
//! correctness: every candidate is verified by the real path matcher, so results equal a scan
//! of every path. Built once per manifest (ref and changeset) and shared.

use std::collections::BTreeMap;

use bee_core::RepoPath;

use crate::plan::TrigramQuery;
use crate::store::Manifest;
use crate::trigram::{Trigram, trigrams_of};

/// Trigram postings over the paths of one manifest.
#[derive(Clone, Debug, Default)]
pub struct PathIndex {
    paths: Vec<RepoPath>,
    postings: BTreeMap<Trigram, Vec<u32>>,
}

impl PathIndex {
    /// Indexes every path of `manifest`, in its order.
    pub fn build(manifest: &Manifest) -> PathIndex {
        let mut idx = PathIndex::default();
        for (n, path) in manifest.files.keys().enumerate() {
            for t in trigrams_of(path.as_bytes()) {
                idx.postings.entry(t).or_default().push(n as u32);
            }
            idx.paths.push(path.clone());
        }
        idx
    }

    /// Number of indexed paths.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// True when the manifest has no files.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The path at position `n`.
    pub fn path(&self, n: u32) -> &RepoPath {
        &self.paths[n as usize]
    }

    /// Positions of the paths that may satisfy `q`, ascending; `None` when `q` cannot narrow
    /// the set (every path is a candidate).
    pub fn candidates(&self, q: &TrigramQuery) -> Option<Vec<u32>> {
        match q {
            TrigramQuery::All => None,
            TrigramQuery::None => Some(Vec::new()),
            TrigramQuery::Tri(t) => Some(self.postings.get(t).cloned().unwrap_or_default()),
            TrigramQuery::And(v) => {
                let mut acc: Option<Vec<u32>> = None;
                for c in v {
                    if let Some(set) = self.candidates(c) {
                        acc = Some(match acc {
                            None => set,
                            Some(a) => intersect(&a, &set),
                        });
                        if acc.as_ref().is_some_and(Vec::is_empty) {
                            break;
                        }
                    }
                }
                acc
            }
            TrigramQuery::Or(v) => {
                let mut acc = Vec::new();
                for c in v {
                    acc = union(&acc, &self.candidates(c)?);
                }
                Some(acc)
            }
        }
    }
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = a.iter().chain(b).copied().collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::plan_regex;
    use crate::query::Expr;
    use crate::query::code::{CodeTerm, parse_code_query};
    use crate::store::FileEntry;

    fn manifest(paths: &[&str]) -> Manifest {
        let mut files = BTreeMap::new();
        for p in paths {
            files.insert(
                RepoPath::new(p.as_bytes().to_vec()).unwrap(),
                FileEntry {
                    blob: bee_object::blob_id(p.as_bytes()),
                    mode: bee_core::Mode::File,
                    size: 1,
                    doc: None,
                    language: None,
                },
            );
        }
        Manifest {
            org: bee_store::OrgId(bee_core::Ulid::from_bytes([1; 16])),
            repo: bee_store::RepoId(bee_core::Ulid::from_bytes([2; 16])),
            ref_name: "heads/main".into(),
            changeset: bee_object::blob_id(b"cs"),
            files,
        }
    }

    fn path_query(q: &str) -> (TrigramQuery, regex::bytes::Regex) {
        let parsed = parse_code_query(q).unwrap();
        let Expr::Term(CodeTerm::Path(p)) = parsed.expr else {
            panic!("a path term")
        };
        (
            plan_regex(&p.regex_source(), true).unwrap_or(TrigramQuery::All),
            p.compile(false).unwrap(),
        )
    }

    #[test]
    fn candidates_cover_every_match_and_narrow_the_set() {
        let paths = [
            "billing/charge.rs",
            "src/billing/invoice.rs",
            "rebilling/x.rs",
            "docs/README.md",
            "src/main.rs",
            "src/net/tcp.rs",
            "tools/Billing.py",
        ];
        let m = manifest(&paths);
        let idx = PathIndex::build(&m);
        assert_eq!(idx.len(), paths.len());
        for q in [
            "path:billing/",
            "path:README",
            "path:*.rs",
            "path:/src/**",
            "path:/^src\\/net/",
            "path:tcp",
            "path:nothing_here",
        ] {
            let (plan, re) = path_query(q);
            let brute: Vec<u32> = (0..idx.len() as u32)
                .filter(|n| re.is_match(idx.path(*n).as_bytes()))
                .collect();
            match idx.candidates(&plan) {
                Some(c) => {
                    for n in &brute {
                        assert!(c.contains(n), "{q}: {} missing", idx.path(*n));
                    }
                    assert!(c.len() <= idx.len());
                }
                None => assert!(q.contains('*') || q.contains('^'), "{q} should plan"),
            }
        }
        // A literal narrows to the paths that contain its trigrams.
        let (plan, _) = path_query("path:billing/");
        let c = idx.candidates(&plan).unwrap();
        let names: Vec<String> = c.iter().map(|n| idx.path(*n).to_string()).collect();
        assert_eq!(
            names,
            vec![
                "billing/charge.rs",
                "rebilling/x.rs",
                "src/billing/invoice.rs"
            ],
            "tools/Billing.py lacks the trigram of the slash"
        );
        let (plan, _) = path_query("path:nothing_here");
        assert_eq!(idx.candidates(&plan), Some(Vec::new()));
    }
}
