//! The path index (docs/spec/search.md §3.3): `path:` queries go through a trigram index over
//! each manifest's paths and return exactly what a scan of every path returns, combined with
//! content, `lang:` and negated terms; the index is built once per manifest.

use crate::common;
use bee_core::RepoPath;
use bee_store::RepoId;
use common::{World, allow_all};

async fn world() -> World {
    let mut w = World::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for d in [
        "billing",
        "src/billing",
        "rebilling",
        "src/net",
        "docs",
        "tools",
    ] {
        for i in 0..20 {
            files.push((
                format!("{d}/f{i}.rs"),
                format!("fn f{i}() {{ /* {d} */ }}\n"),
            ));
            files.push((format!("{d}/n{i}.md"), format!("# {d} {i}\n")));
        }
    }
    files.push(("README.md".into(), "# readme\n".into()));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    w.repo("acme", "core", &refs).await;
    w
}

/// Every file of the repo whose path the query's path rule accepts, by brute force.
fn brute(w: &World, keep: impl Fn(&str) -> bool) -> Vec<String> {
    let t = &w.targets[0];
    let m = w
        .store
        .manifest(t.org, t.repo, &t.ref_name)
        .unwrap()
        .unwrap();
    let mut v: Vec<String> = m
        .files
        .keys()
        .map(|p| p.to_string())
        .filter(|p| keep(p))
        .collect();
    v.sort();
    v
}

fn paths(w: &World, q: &str) -> Vec<String> {
    let auth = |r: RepoId, p: &RepoPath| allow_all(r, p);
    let mut v: Vec<String> = w
        .search(q, &auth)
        .iter()
        .map(|h| h.path.to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn path_queries_through_the_index_equal_a_full_scan() {
    let w = world().await;
    let seg = |p: &str, s: &str| p.starts_with(s) || p.contains(&format!("/{s}"));
    assert_eq!(
        paths(&w, "path:billing/"),
        brute(&w, |p| seg(p, "billing/"))
    );
    assert_eq!(paths(&w, "path:README"), brute(&w, |p| seg(p, "README")));
    assert_eq!(
        paths(&w, "path:/src/**"),
        brute(&w, |p| p.starts_with("src/"))
    );
    assert_eq!(paths(&w, "path:*.md"), brute(&w, |p| p.ends_with(".md")));
    assert_eq!(
        paths(&w, "path:net/ OR path:docs/"),
        brute(&w, |p| seg(p, "net/") || seg(p, "docs/"))
    );
    assert_eq!(
        paths(&w, "path:billing/ -path:src/"),
        brute(&w, |p| seg(p, "billing/") && !seg(p, "src/"))
    );
    // Combined with a content term and a language.
    assert_eq!(
        paths(&w, "path:billing/ f3 lang:rust"),
        vec!["billing/f3.rs".to_string(), "src/billing/f3.rs".to_string()]
    );
    assert!(paths(&w, "path:nowhere_at_all").is_empty());
    // Built once per manifest and reused.
    let t = &w.targets[0];
    let m = w
        .store
        .manifest(t.org, t.repo, &t.ref_name)
        .unwrap()
        .unwrap();
    let a = w.searcher.path_index(&m);
    let b = w.searcher.path_index(&m);
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    assert_eq!(a.len(), m.files.len());
}
