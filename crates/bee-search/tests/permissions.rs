//! Permission matrix (PRODUCT §4.4 "done when"): results never include, count, preview or
//! suggest content the caller cannot read (SPEC §9.3), dedup included.

use crate::common;
use std::collections::BTreeMap;

use bee_core::{Reason, RepoPath};
use bee_search::search::{CodeSearchRequest, PathAuthorizer, RepoTarget, SuggestionKind};
use bee_store::RepoId;
use common::World;

const SECRET: &str = "TOPSECRET_launch_codes";

/// A principal's read rule: which `(repo full name, path prefix)` pairs are denied, and which
/// repos are invisible entirely.
#[derive(Clone)]
struct Principal {
    name: &'static str,
    denied: Vec<(&'static str, &'static str)>,
    hidden_repos: Vec<&'static str>,
}

struct Auth<'a> {
    p: &'a Principal,
    names: BTreeMap<RepoId, String>,
}

impl PathAuthorizer for Auth<'_> {
    fn can_read(&self, repo: RepoId, path: &RepoPath) -> bool {
        let name = &self.names[&repo];
        !self
            .p
            .denied
            .iter()
            .any(|(r, prefix)| r == name && path.to_string().starts_with(prefix))
    }
}

fn principals() -> Vec<Principal> {
    vec![
        Principal {
            name: "admin",
            denied: vec![],
            hidden_repos: vec![],
        },
        Principal {
            name: "dev",
            denied: vec![("acme/core", "secret/")],
            hidden_repos: vec![],
        },
        Principal {
            name: "contractor",
            denied: vec![("acme/core", "secret/"), ("acme/core", "billing/")],
            hidden_repos: vec!["acme/vault"],
        },
        Principal {
            name: "outsider",
            denied: vec![("acme/core", ""), ("acme/web", "")],
            hidden_repos: vec!["acme/vault"],
        },
    ]
}

async fn world() -> World {
    let mut w = World::new();
    w.repo(
        "acme",
        "core",
        &[
            (
                "secret/keys.rs",
                "// TOPSECRET_launch_codes live here\npub fn topsecret_rotate() {}\nconst SHARED_MARKER: u32 = 1;\n",
            ),
            ("secret/dup.txt", "deduplicated shared blob\n"),
            ("billing/invoice.rs", "pub fn invoice_total() -> u64 { 42 }\n// SHARED_MARKER\n"),
            ("src/lib.rs", "pub fn public_api() {}\n// SHARED_MARKER\n"),
        ],
    )
    .await;
    w.repo(
        "acme",
        "web",
        &[
            ("pub/dup.txt", "deduplicated shared blob\n"),
            ("app.js", "function render() { return 'SHARED_MARKER'; }\n"),
        ],
    )
    .await;
    w.repo(
        "acme",
        "vault",
        &[(
            "vault.rs",
            "// TOPSECRET_launch_codes backup\nfn topsecret_backup() {}\n",
        )],
    )
    .await;
    w
}

fn visible_targets(w: &World, p: &Principal) -> Vec<RepoTarget> {
    w.targets
        .iter()
        .filter(|t| !p.hidden_repos.contains(&t.full_name().as_str()))
        .cloned()
        .collect()
}

fn readable(p: &Principal, repo: &str, path: &str) -> bool {
    !p.hidden_repos.contains(&repo)
        && !p
            .denied
            .iter()
            .any(|(r, prefix)| *r == repo && path.starts_with(prefix))
}

/// Every (repo, path, content) of the world, for brute-force expectations.
fn all_files() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "acme/core",
            "secret/keys.rs",
            "// TOPSECRET_launch_codes live here\npub fn topsecret_rotate() {}\nconst SHARED_MARKER: u32 = 1;\n",
        ),
        ("acme/core", "secret/dup.txt", "deduplicated shared blob\n"),
        (
            "acme/core",
            "billing/invoice.rs",
            "pub fn invoice_total() -> u64 { 42 }\n// SHARED_MARKER\n",
        ),
        (
            "acme/core",
            "src/lib.rs",
            "pub fn public_api() {}\n// SHARED_MARKER\n",
        ),
        ("acme/web", "pub/dup.txt", "deduplicated shared blob\n"),
        (
            "acme/web",
            "app.js",
            "function render() { return 'SHARED_MARKER'; }\n",
        ),
        (
            "acme/vault",
            "vault.rs",
            "// TOPSECRET_launch_codes backup\nfn topsecret_backup() {}\n",
        ),
    ]
}

#[tokio::test]
async fn permission_matrix_no_leak_via_hits_counts_previews_or_facets() {
    let w = world().await;
    let queries = [
        "SHARED_MARKER",
        "TOPSECRET",
        "/topsecret_\\w+/",
        "deduplicated",
        "path:secret/",
        "lang:rust",
        "fn",
        "-SHARED_MARKER path:*.rs",
        "symbol:topsecret_rotate",
        "TOPSECRET OR invoice_total",
    ];
    for p in principals() {
        let names: BTreeMap<RepoId, String> =
            w.targets.iter().map(|t| (t.repo, t.full_name())).collect();
        let auth = Auth { p: &p, names };
        let targets = visible_targets(&w, &p);
        for q in queries {
            let resp = w
                .searcher
                .search_code(
                    &CodeSearchRequest {
                        query: q.into(),
                        context_lines: 10,
                        include_facets: true,
                        page_size: 100,
                        ..CodeSearchRequest::default()
                    },
                    &targets,
                    &auth,
                )
                .unwrap();
            // Every hit is readable.
            for h in &resp.hits {
                assert!(
                    readable(&p, &h.repo_full_name, &h.path.to_string()),
                    "{}: {q}: leaked {}:{}",
                    p.name,
                    h.repo_full_name,
                    h.path
                );
                // Previews (lines and context) never carry unreadable text.
                for l in &h.line_matches {
                    for text in l
                        .context_before
                        .iter()
                        .chain(&l.context_after)
                        .chain([&l.line])
                    {
                        if text.contains(SECRET) {
                            assert!(readable(&p, &h.repo_full_name, &h.path.to_string()));
                        }
                    }
                }
            }
            // The count equals the number of readable files an unrestricted search finds.
            let admin_view = w
                .searcher
                .search_code(
                    &CodeSearchRequest {
                        query: q.into(),
                        page_size: 100,
                        ..CodeSearchRequest::default()
                    },
                    &w.targets,
                    &common::allow_all,
                )
                .unwrap();
            let expected = admin_view
                .hits
                .iter()
                .filter(|h| readable(&p, &h.repo_full_name, &h.path.to_string()))
                .count() as u64;
            assert_eq!(resp.total_count, expected, "{}: {q}: count leaks", p.name);
            assert_eq!(resp.hits.len() as u64, expected);
            // Facet counts add up to the visible count.
            for f in &resp.facets {
                let sum: u64 = f.values.iter().map(|(_, c)| c).sum();
                assert_eq!(sum, expected, "{}: {q}: facet {} leaks", p.name, f.name);
            }
        }
    }
}

#[tokio::test]
async fn dedup_never_makes_a_blob_readable_through_another_path() {
    let w = world().await;
    let p = &principals()[1]; // dev: cannot read acme/core secret/
    let names = w.targets.iter().map(|t| (t.repo, t.full_name())).collect();
    let auth = Auth { p, names };
    let hits = w.search("deduplicated", &auth);
    let got: Vec<(String, String)> = hits
        .iter()
        .map(|h| (h.repo_full_name.clone(), h.path.to_string()))
        .collect();
    // The same blob exists at acme/core:secret/dup.txt; only the readable path appears.
    assert_eq!(
        got,
        vec![("acme/web".to_string(), "pub/dup.txt".to_string())]
    );
    // Sanity: the admin sees both, so the blob really is shared.
    assert_eq!(w.search("deduplicated", &common::allow_all).len(), 2);
    let expect: Vec<_> = all_files()
        .into_iter()
        .filter(|(r, path, c)| c.contains("deduplicated") && readable(p, r, path))
        .collect();
    assert_eq!(expect.len(), 1);
}

#[tokio::test]
async fn no_leak_via_symbols_references_outlines_or_suggestions() {
    let w = world().await;
    for p in principals() {
        let names = w.targets.iter().map(|t| (t.repo, t.full_name())).collect();
        let auth = Auth { p: &p, names };
        let targets = visible_targets(&w, &p);
        let can_see_core_secret = readable(&p, "acme/core", "secret/keys.rs");
        let can_see_vault = readable(&p, "acme/vault", "vault.rs");

        // Suggestions.
        let sugg = w
            .searcher
            .suggest("topsecret", 50, &targets, &auth)
            .unwrap();
        let syms: Vec<&str> = sugg
            .iter()
            .filter(|s| s.kind == SuggestionKind::Symbol)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(
            syms.contains(&"topsecret_rotate"),
            can_see_core_secret,
            "{}",
            p.name
        );
        assert_eq!(
            syms.contains(&"topsecret_backup"),
            can_see_vault,
            "{}",
            p.name
        );
        let paths = w.searcher.suggest("keys", 50, &targets, &auth).unwrap();
        assert_eq!(
            paths.iter().any(|s| s.kind == SuggestionKind::Path),
            can_see_core_secret,
            "{}",
            p.name
        );
        let repos = w.searcher.suggest("vau", 50, &targets, &auth).unwrap();
        assert_eq!(
            repos.iter().any(|s| s.kind == SuggestionKind::Repo),
            !p.hidden_repos.contains(&"acme/vault")
        );

        // Symbol search and definitions.
        let s = w
            .searcher
            .search_symbols("/^topsecret_/", &[], 50, "", &targets, &auth)
            .unwrap();
        let expected = usize::from(can_see_core_secret) + usize::from(can_see_vault);
        assert_eq!(s.symbols.len(), expected, "{}", p.name);
        assert_eq!(s.total_count, expected as u64);
        let d = w
            .searcher
            .definitions("topsecret_rotate", &targets, &auth)
            .unwrap();
        assert_eq!(d.len(), usize::from(can_see_core_secret));

        // References: SHARED_MARKER appears in four files.
        let refs = w
            .searcher
            .references("SHARED_MARKER", true, "", &targets, &auth)
            .unwrap();
        for r in &refs {
            assert!(
                readable(&p, &r.repo_full_name, &r.path.to_string()),
                "{}",
                p.name
            );
        }
        let expect = all_files()
            .into_iter()
            .filter(|(r, path, c)| c.contains("SHARED_MARKER") && readable(&p, r, path))
            .count();
        assert_eq!(refs.len(), expect, "{}", p.name);

        // Outline of an unreadable file is NOT_FOUND, indistinguishable from absent.
        let core = w
            .targets
            .iter()
            .find(|t| t.full_name() == "acme/core")
            .unwrap();
        let out =
            w.searcher
                .document_symbols(core, &RepoPath::new("secret/keys.rs").unwrap(), &auth);
        if can_see_core_secret {
            let (_, _, syms) = out.unwrap();
            assert!(syms.iter().any(|s| s.name == "topsecret_rotate"));
        } else {
            assert_eq!(out.unwrap_err().reason, Reason::NotFound);
        }
        let absent = w
            .searcher
            .document_symbols(core, &RepoPath::new("secret/nope.rs").unwrap(), &auth)
            .unwrap_err();
        assert_eq!(absent.reason, Reason::NotFound);
    }
}
