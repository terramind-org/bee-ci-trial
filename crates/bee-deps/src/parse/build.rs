//! Shared graph construction: turns raw lockfile entries and declared roots into [`Package`]s
//! with direct/indirect relationships, scopes and resolved edges.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::model::{Ecosystem, Package, Relationship, Scope};
use crate::version;

/// A dependency edge as written in a lockfile: a name plus either an exact version or a range.
#[derive(Clone, Debug)]
pub(crate) struct RawDep {
    pub name: String,
    pub req: Option<String>,
}

impl RawDep {
    pub fn new(name: &str, req: Option<&str>) -> RawDep {
        RawDep {
            name: name.to_owned(),
            req: req.map(str::to_owned).filter(|r| !r.is_empty()),
        }
    }
}

/// One package entry read from a lockfile or manifest.
#[derive(Clone, Debug)]
pub(crate) struct Raw {
    pub name: String,
    pub version: String,
    pub deps: Vec<RawDep>,
    pub licenses: Vec<String>,
    pub integrity: Option<String>,
    /// A dev/prod hint carried by the file (`dev: true`, `packages-dev`), used when no root
    /// reaches the package.
    pub dev_hint: Option<bool>,
}

impl Raw {
    pub fn new(name: &str, version: &str) -> Raw {
        Raw {
            name: name.to_owned(),
            version: version.to_owned(),
            deps: Vec::new(),
            licenses: Vec::new(),
            integrity: None,
            dev_hint: None,
        }
    }
}

/// A dependency the project declares itself.
#[derive(Clone, Debug)]
pub(crate) struct Root {
    pub name: String,
    pub requirement: Option<String>,
    pub scope: Scope,
    /// The exact resolved version when the file says which one.
    pub pinned: Option<String>,
}

impl Root {
    pub fn new(name: &str, requirement: Option<&str>, scope: Scope) -> Root {
        Root {
            name: name.to_owned(),
            requirement: requirement
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty()),
            scope,
            pinned: None,
        }
    }

    pub fn pinned(mut self, v: &str) -> Root {
        self.pinned = Some(v.to_owned());
        self
    }
}

/// Picks the best version among `candidates` for `req`: an exact string match, else the highest
/// version satisfying the requirement, else the highest version.
pub(crate) fn pick<'a>(
    eco: Ecosystem,
    candidates: &[&'a str],
    req: Option<&str>,
) -> Option<&'a str> {
    if candidates.is_empty() {
        return None;
    }
    let highest = |list: &[&'a str]| -> Option<&'a str> {
        list.iter()
            .copied()
            .max_by(|a, b| version::compare(eco, a, b))
    };
    if let Some(r) = req {
        if let Some(exact) = candidates.iter().copied().find(|c| *c == r) {
            return Some(exact);
        }
        let ok: Vec<&str> = candidates
            .iter()
            .copied()
            .filter(|c| version::satisfies(eco, c, r) == Some(true))
            .collect();
        if !ok.is_empty() {
            return highest(&ok);
        }
    }
    highest(candidates)
}

/// Builds packages for one source file.
///
/// * `raw` are the resolved entries (empty when only a manifest exists);
/// * `roots` are the project's own declarations;
/// * `has_lock` says whether `raw` is a complete resolution, in which case a root that is not in
///   `raw` is dropped instead of being added as an unresolved package.
pub(crate) fn build(
    eco: Ecosystem,
    source: &str,
    raw: Vec<Raw>,
    roots: Vec<Root>,
    has_lock: bool,
) -> Vec<Package> {
    // Merge duplicate (name, version) entries.
    let mut merged: BTreeMap<(String, String), Raw> = BTreeMap::new();
    for r in raw {
        let key = (eco.normalize_name(&r.name), r.version.clone());
        match merged.get_mut(&key) {
            Some(e) => {
                e.deps.extend(r.deps);
                e.licenses.extend(r.licenses);
                if e.integrity.is_none() {
                    e.integrity = r.integrity;
                }
                e.dev_hint = match (e.dev_hint, r.dev_hint) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (a, b) => a.or(b),
                };
            }
            None => {
                merged.insert(key, r);
            }
        }
    }
    let mut entries: Vec<Raw> = merged.into_values().collect();
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        by_name
            .entry(eco.normalize_name(&e.name))
            .or_default()
            .push(i);
    }

    // Resolve a (name, req) pair to an entry index.
    fn resolve(
        eco: Ecosystem,
        by_name: &BTreeMap<String, Vec<usize>>,
        entries: &[Raw],
        name: &str,
        req: Option<&str>,
    ) -> Option<usize> {
        let idxs = by_name.get(&eco.normalize_name(name))?;
        let versions: Vec<&str> = idxs.iter().map(|i| entries[*i].version.as_str()).collect();
        let v = pick(eco, &versions, req)?;
        idxs.iter().copied().find(|i| entries[*i].version == v)
    }

    // Add unresolved packages for roots that have no lock entry.
    let mut root_idx: Vec<(usize, Scope, Option<String>)> = Vec::new();
    let mut unresolved: BTreeSet<usize> = BTreeSet::new();
    for root in &roots {
        let found = match &root.pinned {
            Some(p) => by_name
                .get(&eco.normalize_name(&root.name))
                .and_then(|idxs| idxs.iter().copied().find(|i| entries[*i].version == *p)),
            None => resolve(
                eco,
                &by_name,
                &entries,
                &root.name,
                root.requirement.as_deref(),
            ),
        };
        match found {
            Some(i) => root_idx.push((i, root.scope, root.requirement.clone())),
            None if !has_lock => {
                let v = root
                    .pinned
                    .clone()
                    .or_else(|| {
                        root.requirement
                            .as_deref()
                            .and_then(|r| version::min_version(eco, r))
                    })
                    .unwrap_or_default();
                let mut r = Raw::new(&root.name, &v);
                r.dev_hint = Some(root.scope == Scope::Development);
                entries.push(r);
                let i = entries.len() - 1;
                by_name
                    .entry(eco.normalize_name(&root.name))
                    .or_default()
                    .push(i);
                if root.pinned.is_none() {
                    unresolved.insert(i);
                }
                root_idx.push((i, root.scope, root.requirement.clone()));
            }
            None => {}
        }
    }

    // Edges.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (i, e) in entries.iter().enumerate() {
        let mut out: Vec<usize> = e
            .deps
            .iter()
            .filter_map(|d| resolve(eco, &by_name, &entries, &d.name, d.req.as_deref()))
            .filter(|j| *j != i)
            .collect();
        out.sort_unstable();
        out.dedup();
        edges[i] = out;
    }

    // Roots: declared ones, or packages nothing else depends on.
    let mut direct: BTreeMap<usize, (Scope, Option<String>)> = BTreeMap::new();
    for (i, scope, req) in &root_idx {
        let slot = direct.entry(*i).or_insert((*scope, req.clone()));
        if *scope == Scope::Runtime {
            slot.0 = Scope::Runtime;
        }
        if slot.1.is_none() {
            slot.1 = req.clone();
        }
    }
    if root_idx.is_empty() {
        let mut indeg = vec![0usize; entries.len()];
        for out in &edges {
            for j in out {
                indeg[*j] += 1;
            }
        }
        for (i, e) in entries.iter().enumerate() {
            if indeg[i] == 0 {
                let scope = if e.dev_hint == Some(true) {
                    Scope::Development
                } else {
                    Scope::Runtime
                };
                direct.insert(i, (scope, None));
            }
        }
    }

    // Scope propagation: runtime roots first, then development roots.
    let mut scope_of: Vec<Option<Scope>> = vec![None; entries.len()];
    for want in [Scope::Runtime, Scope::Development] {
        let mut queue: VecDeque<usize> = direct
            .iter()
            .filter(|(_, (s, _))| *s == want)
            .map(|(i, _)| *i)
            .collect();
        while let Some(i) = queue.pop_front() {
            if scope_of[i].is_some() {
                continue;
            }
            scope_of[i] = Some(want);
            for j in &edges[i] {
                if scope_of[*j].is_none() {
                    queue.push_back(*j);
                }
            }
        }
    }

    let purls: Vec<String> = entries
        .iter()
        .map(|e| eco.purl(&e.name, &e.version).to_string())
        .collect();
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let d = direct.get(&i);
            let scope = scope_of[i].unwrap_or(if e.dev_hint == Some(true) {
                Scope::Development
            } else {
                Scope::Runtime
            });
            Package {
                ecosystem: eco,
                name: e.name.clone(),
                version: e.version.clone(),
                purl: purls[i].clone(),
                scope,
                relationship: if d.is_some() {
                    Relationship::Direct
                } else {
                    Relationship::Indirect
                },
                source_path: source.to_owned(),
                requirement: d.and_then(|(_, r)| r.clone()),
                resolved: !unresolved.contains(&i) && !e.version.is_empty(),
                dependencies: edges[i].iter().map(|j| purls[*j].clone()).collect(),
                licenses: e.licenses.clone(),
                integrity: e.integrity.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_and_relationships_propagate() {
        let mut a = Raw::new("a", "1.0.0");
        a.deps = vec![RawDep::new("c", Some("^1.0.0"))];
        let mut b = Raw::new("b", "2.0.0");
        b.deps = vec![RawDep::new("c", None), RawDep::new("d", None)];
        let c = Raw::new("c", "1.2.0");
        let d = Raw::new("d", "0.1.0");
        let roots = vec![
            Root::new("a", Some("^1.0.0"), Scope::Runtime),
            Root::new("b", Some("^2"), Scope::Development),
        ];
        let pk = build(
            Ecosystem::Npm,
            "package-lock.json",
            vec![a, b, c, d],
            roots,
            true,
        );
        let get = |n: &str| pk.iter().find(|p| p.name == n).unwrap();
        assert_eq!(get("a").relationship, Relationship::Direct);
        assert_eq!(get("c").relationship, Relationship::Indirect);
        assert_eq!(get("c").scope, Scope::Runtime);
        assert_eq!(get("d").scope, Scope::Development);
        assert_eq!(get("b").scope, Scope::Development);
        assert_eq!(get("a").dependencies, vec![get("c").purl.clone()]);
    }

    #[test]
    fn manifest_only_roots_become_unresolved_packages() {
        let roots = vec![
            Root::new("serde", Some("1.0.150"), Scope::Runtime),
            Root::new("tokio", Some("1"), Scope::Development),
        ];
        let pk = build(Ecosystem::Cargo, "Cargo.toml", vec![], roots, false);
        assert_eq!(pk.len(), 2);
        let tokio = pk.iter().find(|p| p.name == "tokio").unwrap();
        assert_eq!(tokio.version, "1.0.0");
        assert!(!tokio.resolved);
        assert_eq!(tokio.scope, Scope::Development);
    }

    #[test]
    fn no_roots_marks_unreferenced_packages_direct() {
        let mut a = Raw::new("a", "1");
        a.deps = vec![RawDep::new("b", None)];
        let b = Raw::new("b", "1");
        let pk = build(Ecosystem::Npm, "yarn.lock", vec![a, b], vec![], true);
        assert_eq!(
            pk.iter()
                .filter(|p| p.relationship == Relationship::Direct)
                .count(),
            1
        );
    }

    #[test]
    fn pick_prefers_exact_then_satisfying_then_highest() {
        let c = ["1.0.0", "1.5.0", "2.0.0"];
        assert_eq!(pick(Ecosystem::Npm, &c, Some("1.0.0")), Some("1.0.0"));
        assert_eq!(pick(Ecosystem::Npm, &c, Some("^1.0.0")), Some("1.5.0"));
        assert_eq!(pick(Ecosystem::Npm, &c, Some("^9.0.0")), Some("2.0.0"));
        assert_eq!(pick(Ecosystem::Npm, &c, None), Some("2.0.0"));
    }
}
