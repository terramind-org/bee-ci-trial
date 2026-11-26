//! Ranking order, determinism across runs, stores and target order, and stable page
//! tokens.

use crate::common;
use std::collections::BTreeMap;
use std::sync::Arc;

use bee_core::{Reason, Ulid};
use bee_search::search::{CodeSearchRequest, RepoTarget, Searcher};
use bee_search::store::IndexStore;
use bee_search::{DiskIndexStore, MemoryIndexStore};
use bee_store::{OrgId, RepoId};
use common::{all_hits, allow_all, index_files};

fn corpus() -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    m.insert("a/b/c/deep_exact.rs".into(), b"let x = Invoice;\n".to_vec());
    m.insert("top_exact.rs".into(), b"Invoice::new()\n".to_vec());
    m.insert("word_case.rs".into(), b"let invoice = 1;\n".to_vec());
    m.insert("substring.rs".into(), b"InvoiceLine::new()\n".to_vec());
    m.insert("x/substring2.rs".into(), b"my_invoices\n".to_vec());
    for i in 0..30 {
        m.insert(
            format!("bulk/f{i:02}.txt"),
            format!("invoice {i}\n").into_bytes(),
        );
    }
    m
}

fn build(store: Arc<dyn IndexStore>) -> (Searcher, Vec<RepoTarget>) {
    let org = OrgId(Ulid::from_u128(5));
    let mut targets = vec![
        index_files(store.as_ref(), org, RepoId(Ulid::from_u128(1)), &corpus()),
        index_files(store.as_ref(), org, RepoId(Ulid::from_u128(2)), &corpus()),
    ];
    targets[1].repo_name = "zz".into();
    (Searcher::new(store), targets)
}

fn order(searcher: &Searcher, targets: &[RepoTarget], q: &str) -> Vec<(String, String, f32)> {
    all_hits(searcher, q, targets, &allow_all)
        .into_iter()
        .map(|h| (h.repo_full_name, h.path.to_string(), h.score))
        .collect()
}

#[test]
fn exact_beats_word_boundary_beats_substring_then_depth() {
    let (s, t) = build(Arc::new(MemoryIndexStore::new()));
    let hits = order(&s, &t[..1], "Invoice");
    let paths: Vec<&str> = hits.iter().map(|h| h.1.as_str()).collect();
    // Exact text first (shallow before deep), then case-folded whole words, then substrings.
    assert_eq!(&paths[..2], &["top_exact.rs", "a/b/c/deep_exact.rs"]);
    let pos = |p: &str| paths.iter().position(|x| *x == p).unwrap();
    assert!(pos("word_case.rs") < pos("substring.rs"));
    assert!(pos("bulk/f00.txt") < pos("substring.rs"));
    assert!(pos("substring.rs") < pos("x/substring2.rs"));
    // Scores never increase down the list.
    assert!(hits.windows(2).all(|w| w[0].2 >= w[1].2));
}

#[test]
fn ranking_is_deterministic_across_runs_stores_and_target_order() {
    let queries = [
        "invoice",
        "/inv[a-z]+/",
        "Invoice lang:rust",
        "path:bulk/ invoice",
        "-zebra path:*.rs",
    ];
    let (mem, t) = build(Arc::new(MemoryIndexStore::new()));
    let dir = tempfile::tempdir().unwrap();
    let (disk, t2) = build(Arc::new(DiskIndexStore::open(dir.path()).unwrap()));
    let mut reversed = t.clone();
    reversed.reverse();
    for q in queries {
        let a = order(&mem, &t, q);
        assert!(!a.is_empty(), "{q}");
        assert_eq!(a, order(&mem, &t, q), "{q}: run to run");
        assert_eq!(a, order(&disk, &t2, q), "{q}: memory vs disk");
        assert_eq!(a, order(&mem, &reversed, q), "{q}: target order");
    }
}

#[test]
fn page_tokens_are_stable_and_bound_to_the_query() {
    let (s, t) = build(Arc::new(MemoryIndexStore::new()));
    let req = |token: String, size: u32| CodeSearchRequest {
        query: "invoice".into(),
        page_size: size,
        page_token: token,
        ..CodeSearchRequest::default()
    };
    let all = s
        .search_code(&req(String::new(), 100), &t, &allow_all)
        .unwrap();
    assert_eq!(all.total_count, 70);
    assert!(all.next_page_token.is_none());
    // Paging in fives yields the same sequence.
    let mut paged = Vec::new();
    let mut token = String::new();
    loop {
        let r = s
            .search_code(&req(token.clone(), 5), &t, &allow_all)
            .unwrap();
        assert_eq!(r.total_count, 70);
        paged.extend(r.hits.into_iter().map(|h| (h.repo_full_name, h.path)));
        match r.next_page_token {
            Some(n) => token = n,
            None => break,
        }
    }
    let full: Vec<_> = all
        .hits
        .into_iter()
        .map(|h| (h.repo_full_name, h.path))
        .collect();
    assert_eq!(paged, full);

    // A token is a position, not an offset: hits removed before it do not shift the page.
    let first = s
        .search_code(&req(String::new(), 5), &t, &allow_all)
        .unwrap();
    let tok = first.next_page_token.unwrap();
    let fewer = &t[1..];
    let next = s
        .search_code(&req(tok.clone(), 5), fewer, &allow_all)
        .unwrap();
    for h in &next.hits {
        assert!(
            !first
                .hits
                .iter()
                .any(|f| f.repo_full_name == h.repo_full_name && f.path == h.path)
        );
    }

    // Tokens from another query, or garbage, are rejected.
    let other = s
        .search_code(
            &CodeSearchRequest {
                query: "Invoice case:yes".into(),
                page_token: tok,
                ..CodeSearchRequest::default()
            },
            &t,
            &allow_all,
        )
        .unwrap_err();
    assert_eq!(other.reason, Reason::InvalidArgument);
    let junk = s
        .search_code(&req("zz".into(), 5), &t, &allow_all)
        .unwrap_err();
    assert_eq!(junk.reason, Reason::InvalidArgument);
}

#[test]
fn previews_context_facets_and_streaming() {
    let store: Arc<dyn IndexStore> = Arc::new(MemoryIndexStore::new());
    let mut files = BTreeMap::new();
    files.insert(
        "src/lib.rs".to_string(),
        b"line one\nline two\nfn target() {}\nline four\nline five\n".to_vec(),
    );
    files.insert("doc.md".to_string(), b"see target\n".to_vec());
    let t = vec![index_files(
        store.as_ref(),
        OrgId(Ulid::from_u128(9)),
        RepoId(Ulid::from_u128(3)),
        &files,
    )];
    let s = Searcher::new(store);
    let r = s
        .search_code(
            &CodeSearchRequest {
                query: "target".into(),
                context_lines: 1,
                include_facets: true,
                ..CodeSearchRequest::default()
            },
            &t,
            &allow_all,
        )
        .unwrap();
    let lib = r
        .hits
        .iter()
        .find(|h| h.path.to_string() == "src/lib.rs")
        .unwrap();
    let lm = &lib.line_matches[0];
    assert_eq!(lm.line_number, 3);
    assert_eq!(lm.column, 3);
    assert_eq!(lm.line, "fn target() {}");
    assert_eq!(lm.context_before, vec!["line two"]);
    assert_eq!(lm.context_after, vec!["line four"]);
    assert_eq!((lm.ranges[0].start, lm.ranges[0].end), (3, 9));
    let langs = r.facets.iter().find(|f| f.name == "language").unwrap();
    assert_eq!(
        langs.values,
        vec![("markdown".to_string(), 1), ("rust".to_string(), 1)]
    );

    let mut streamed = Vec::new();
    let summary = s
        .stream_code(
            &CodeSearchRequest {
                query: "line".into(),
                ..CodeSearchRequest::default()
            },
            2,
            &t,
            &allow_all,
            &mut |h| streamed.push(h),
        )
        .unwrap();
    assert_eq!(summary.files_matched, 1);
    assert_eq!(summary.matches, 4);
    assert!(!summary.limit_hit);
    let symbol = s
        .search_symbols("target", &[], 10, "", &t, &allow_all)
        .unwrap();
    assert_eq!(symbol.symbols.len(), 1);
    assert_eq!(symbol.symbols[0].line_number, 3);
}
