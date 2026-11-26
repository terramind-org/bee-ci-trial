//! Index results equal a brute-force scan of every file, for literal, regex and boolean
//! queries over generated corpora (the trigram planner never drops a true match).

use crate::common;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_core::Ulid;
use bee_search::MemoryIndexStore;
use bee_search::search::{RepoTarget, Searcher};
use bee_search::store::IndexStore;
use bee_store::{OrgId, RepoId};
use proptest::prelude::*;
use regex::bytes::RegexBuilder;

const TOKENS: &[&str] = &[
    "foo", "bar", "Foo", "BAR", "baz", "qux", "fo", "o", "_", "(", ")", " ", " ", "\n", "\n", "é",
    "É", "ſ", "s", "x1", "foo_bar", "\t", ".", "q",
];

const REGEXES: &[&str] = &[
    "fo+",
    "b[a-z]r",
    r"(foo|qux)\(",
    "^foo",
    "bar$",
    r"\bbaz\b",
    "f.o",
    "qu?x",
    "(?i)foo",
    "é|ſ",
    "o{2,}",
    r"x\d",
    "[Ff]oo_?[Bb]",
    r"\(\)",
    "ba[rz]+",
    "(?-i)Foo",
    r"foo\s+bar",
    ".",
];

#[derive(Clone, Debug)]
enum Term {
    Lit(String),
    Re(String),
}

impl Term {
    fn text(&self) -> String {
        match self {
            Term::Lit(l) => format!("\"{l}\""),
            Term::Re(r) => format!("/{r}/"),
        }
    }

    fn regex(&self, case_sensitive: bool) -> regex::bytes::Regex {
        let src = match self {
            Term::Lit(l) => regex::escape(l),
            Term::Re(r) => r.clone(),
        };
        RegexBuilder::new(&src)
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .build()
            .unwrap()
    }
}

#[derive(Clone, Debug)]
enum Shape {
    One(Term),
    And(Term, Term),
    Or(Term, Term),
    AndNot(Term, Term),
}

fn term() -> impl Strategy<Value = Term> {
    prop_oneof![
        prop::collection::vec(prop::sample::select(TOKENS), 1..3).prop_filter_map(
            "printable literal",
            |v| {
                let s: String = v.concat();
                (!s.trim().is_empty() && !s.contains('\n') && !s.contains('\t'))
                    .then_some(Term::Lit(s))
            }
        ),
        prop::sample::select(REGEXES).prop_map(|r| Term::Re(r.to_string())),
    ]
}

fn shape() -> impl Strategy<Value = Shape> {
    prop_oneof![
        term().prop_map(Shape::One),
        (term(), term()).prop_map(|(a, b)| Shape::And(a, b)),
        (term(), term()).prop_map(|(a, b)| Shape::Or(a, b)),
        (term(), term()).prop_map(|(a, b)| Shape::AndNot(a, b)),
    ]
}

fn content() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(TOKENS), 0..60).prop_map(|v| v.concat().into_bytes())
}

fn corpus() -> impl Strategy<Value = Vec<BTreeMap<String, Vec<u8>>>> {
    let file = (
        prop::sample::select(&["src", "lib/a", "docs", "x/y/z"][..]),
        prop::sample::select(&["main", "util", "Foo", "bar_baz"][..]),
        prop::sample::select(&["rs", "py", "md", "go"][..]),
        content(),
    )
        .prop_map(|(d, n, e, c)| (format!("{d}/{n}.{e}"), c));
    prop::collection::vec(
        prop::collection::btree_map(file.prop_map(|(p, c)| (p, c)), Just(()), 1..8)
            .prop_map(|m| m.into_keys().collect::<BTreeMap<String, Vec<u8>>>()),
        1..4,
    )
}

/// Brute force: per file, does the query hold, and which match starts does it report.
fn brute(shape: &Shape, cs: bool, content: &[u8]) -> Option<BTreeSet<(usize, usize)>> {
    let ranges = |t: &Term| -> Vec<(usize, usize)> {
        t.regex(cs)
            .find_iter(content)
            .map(|m| (m.start(), m.end()))
            .collect()
    };
    let mut out = BTreeSet::new();
    let ok = match shape {
        Shape::One(a) => {
            let r = ranges(a);
            out.extend(r.iter().copied());
            !r.is_empty()
        }
        Shape::And(a, b) => {
            let (ra, rb) = (ranges(a), ranges(b));
            out.extend(ra.iter().copied());
            out.extend(rb.iter().copied());
            !ra.is_empty() && !rb.is_empty()
        }
        Shape::Or(a, b) => {
            let (ra, rb) = (ranges(a), ranges(b));
            out.extend(ra.iter().copied());
            out.extend(rb.iter().copied());
            !ra.is_empty() || !rb.is_empty()
        }
        Shape::AndNot(a, b) => {
            let (ra, rb) = (ranges(a), ranges(b));
            out.extend(ra.iter().copied());
            !ra.is_empty() && rb.is_empty()
        }
    };
    ok.then_some(out)
}

fn query_text(shape: &Shape, cs: bool) -> String {
    let q = match shape {
        Shape::One(a) => a.text(),
        Shape::And(a, b) => format!("{} {}", a.text(), b.text()),
        Shape::Or(a, b) => format!("{} OR {}", a.text(), b.text()),
        Shape::AndNot(a, b) => format!("{} -{}", a.text(), b.text()),
    };
    if cs { format!("{q} case:yes") } else { q }
}

fn line_of(content: &[u8], at: usize) -> u32 {
    content[..at].iter().filter(|b| **b == b'\n').count() as u32 + 1
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 192, ..ProptestConfig::default() })]

    #[test]
    fn index_equals_brute_force(repos in corpus(), shape in shape(), cs in any::<bool>()) {
        let store = Arc::new(MemoryIndexStore::new());
        let org = OrgId(Ulid::from_u128(1));
        let targets: Vec<RepoTarget> = repos
            .iter()
            .enumerate()
            .map(|(i, files)| {
                common::index_files(store.as_ref(), org, RepoId(Ulid::from_u128(10 + i as u128)), files)
            })
            .collect();
        let searcher = Searcher::new(store.clone() as Arc<dyn IndexStore>);
        let q = query_text(&shape, cs);
        let hits = common::all_hits(&searcher, &q, &targets, &common::allow_all);

        // Expected: (repo, path) → set of (line, column) of match starts, plus counts.
        let mut expected: BTreeMap<(String, String), (u32, BTreeSet<u32>)> = BTreeMap::new();
        for (t, files) in targets.iter().zip(&repos) {
            for (path, c) in files {
                if let Some(ranges) = brute(&shape, cs, c) {
                    let lines = ranges.iter().map(|(s, _)| line_of(c, *s)).collect();
                    expected.insert((t.full_name(), path.clone()), (ranges.len() as u32, lines));
                }
            }
        }
        let mut got: BTreeMap<(String, String), (u32, BTreeSet<u32>)> = BTreeMap::new();
        for h in &hits {
            let lines = h.line_matches.iter().map(|l| l.line_number).collect();
            let prev = got.insert((h.repo_full_name.clone(), h.path.to_string()), (h.match_count, lines));
            prop_assert!(prev.is_none(), "duplicate hit across pages: {:?}", h.path);
        }
        prop_assert_eq!(got, expected, "query {}", q);
    }

    #[test]
    fn literal_case_sensitive_equals_naive_find(
        repos in corpus(),
        lit in prop::collection::vec(prop::sample::select(&["foo", "bar", "o", "_", "ba", "Fo", "x1"][..]), 1..3),
    ) {
        let lit: String = lit.concat();
        let store = Arc::new(MemoryIndexStore::new());
        let org = OrgId(Ulid::from_u128(2));
        let targets: Vec<RepoTarget> = repos
            .iter()
            .enumerate()
            .map(|(i, files)| common::index_files(store.as_ref(), org, RepoId(Ulid::from_u128(50 + i as u128)), files))
            .collect();
        let searcher = Searcher::new(store.clone() as Arc<dyn IndexStore>);
        let hits = common::all_hits(&searcher, &format!("\"{lit}\" case:yes"), &targets, &common::allow_all);
        let got: BTreeSet<(String, String)> =
            hits.iter().map(|h| (h.repo_full_name.clone(), h.path.to_string())).collect();
        let mut expected = BTreeSet::new();
        for (t, files) in targets.iter().zip(&repos) {
            for (path, c) in files {
                if c.windows(lit.len()).any(|w| w == lit.as_bytes()) {
                    expected.insert((t.full_name(), path.clone()));
                }
            }
        }
        prop_assert_eq!(got, expected);
    }
}
