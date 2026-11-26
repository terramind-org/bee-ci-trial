//! Agent scenarios AS-SEARCH-01 and AS-SEARCH-02 (PRODUCT §4.4).

use crate::common;
use std::ops::Bound;

use bee_search::forge::{
    Filter, FilterContext, ForgeKind, Key, Predicate, SortOrder, parse_forge_query,
};
use bee_search::search::CodeSearchRequest;
use bee_testkit::Change;
use common::{World, allow_all};

#[tokio::test]
async fn as_search_01_find_every_call_site_across_repos() {
    let mut w = World::new();
    w.repo(
        "acme",
        "billing",
        &[
            (
                "src/charge.rs",
                "pub fn charge_card(amount: u64) -> bool {\n    amount > 0\n}\n",
            ),
            (
                "src/checkout.rs",
                "use crate::charge::charge_card;\n\nfn checkout() {\n    if charge_card(10) {\n        log();\n    }\n    charge_card(20);\n}\n",
            ),
            ("README.md", "Call charge_card to bill.\n"),
        ],
    )
    .await;
    w.repo(
        "acme",
        "web",
        &[
            (
                "app/pay.py",
                "def pay():\n    return billing.charge_card(5)\n",
            ),
            ("app/other.py", "def recharge_cards():\n    pass\n"),
        ],
    )
    .await;
    w.repo(
        "other",
        "tools",
        &[("x.go", "func main() { charge_card(1) }\n")],
    )
    .await;

    // Step 1: find the definition.
    let defs = w
        .searcher
        .definitions("charge_card", &w.targets, &allow_all)
        .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].path.to_string(), "src/charge.rs");
    assert_eq!(defs[0].line_number, 1);

    // Step 2: every call site in the org, as an agent would query it.
    let hits = w.search(r"/\bcharge_card\(/ org:acme", &allow_all);
    let mut sites: Vec<(String, String, u32)> = hits
        .iter()
        .flat_map(|h| {
            h.line_matches
                .iter()
                .map(|l| (h.repo_full_name.clone(), h.path.to_string(), l.line_number))
                .collect::<Vec<_>>()
        })
        .collect();
    sites.sort();
    assert_eq!(
        sites,
        vec![
            ("acme/billing".into(), "src/charge.rs".into(), 1),
            ("acme/billing".into(), "src/checkout.rs".into(), 4),
            ("acme/billing".into(), "src/checkout.rs".into(), 7),
            ("acme/web".into(), "app/pay.py".into(), 2),
        ]
    );
    assert!(hits.iter().all(|h| !h.repo_full_name.starts_with("other/")));

    // The search-based references API gives the same call sites, minus the definition, and
    // skips `recharge_cards` (word boundary) and the import line only when filtered out.
    let refs = w
        .searcher
        .references(
            "charge_card",
            false,
            "org:acme lang:rust OR lang:python",
            &w.targets,
            &allow_all,
        )
        .unwrap();
    let mut got: Vec<(String, u32, u32)> = refs
        .iter()
        .map(|r| (r.path.to_string(), r.line_number, r.column))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("app/pay.py".into(), 2, 19),
            ("src/checkout.rs".into(), 1, 19),
            ("src/checkout.rs".into(), 4, 7),
            ("src/checkout.rs".into(), 7, 4),
        ]
    );

    // Step 3: one change fixing them all (rename), then the index sees no call site left.
    w.change(
        "acme/billing",
        &[
            Change::put("src/charge.rs", "pub fn charge(amount: u64) -> bool {\n    amount > 0\n}\n"),
            Change::put(
                "src/checkout.rs",
                "use crate::charge::charge;\n\nfn checkout() {\n    if charge(10) {\n        log();\n    }\n    charge(20);\n}\n",
            ),
        ],
    )
    .await;
    w.change(
        "acme/web",
        &[Change::put(
            "app/pay.py",
            "def pay():\n    return billing.charge(5)\n",
        )],
    )
    .await;
    let left = w.search(r"/\bcharge_card\(/ org:acme", &allow_all);
    assert!(left.is_empty(), "{left:?}");
    let resp = w
        .searcher
        .search_code(
            &CodeSearchRequest {
                query: "charge_card".into(),
                ..CodeSearchRequest::default()
            },
            &w.targets,
            &allow_all,
        )
        .unwrap();
    let mut remaining: Vec<String> = resp.hits.iter().map(|h| h.path.to_string()).collect();
    remaining.sort();
    // A plain literal is a substring match, so `recharge_cards` still matches.
    assert_eq!(remaining, vec!["README.md", "app/other.py", "x.go"]);
}

#[test]
fn as_search_02_parse_waiting_review_query_into_filter() {
    let q = parse_forge_query(
        ForgeKind::Issues,
        "is:open is:cr review:required path:billing/ sort:updated-asc",
    )
    .unwrap();
    assert_eq!(
        q.normalized(),
        "is:open is:cr review:required path:billing/ sort:updated-asc"
    );
    let f = q.to_filter(&FilterContext::default()).unwrap();
    assert_eq!(f.sort.field, "updated");
    assert_eq!(f.sort.order, SortOrder::Asc);
    assert_eq!(
        f.filter,
        Filter::And(vec![
            Filter::Pred(Predicate::Enum {
                field: "state",
                value: "open"
            }),
            Filter::Pred(Predicate::Enum {
                field: "type",
                value: "cr"
            }),
            Filter::Pred(Predicate::Enum {
                field: "review",
                value: "required"
            }),
            Filter::Pred(Predicate::PathPrefix("billing/".into())),
        ])
    );

    // The GitHub spelling `is:pr` means the same thing, and `@me`/dates resolve from the
    // context only.
    let q2 = parse_forge_query(
        ForgeKind::Issues,
        "is:pr state:open review:required path:/billing/ review-requested:@me updated:>=2026-01-01",
    )
    .unwrap();
    assert!(q2.to_filter(&FilterContext::default()).is_err());
    let f2 = q2
        .to_filter(&FilterContext {
            me: Some("agent-7".into()),
            today: None,
        })
        .unwrap();
    let Filter::And(parts) = f2.filter else {
        panic!("conjunction expected")
    };
    assert_eq!(
        parts[0],
        Filter::Pred(Predicate::Enum {
            field: "type",
            value: "cr"
        })
    );
    assert_eq!(
        parts[3],
        Filter::Pred(Predicate::PathPrefix("billing/".into()))
    );
    assert_eq!(
        parts[4],
        Filter::Pred(Predicate::Person {
            role: Key::ReviewRequested,
            login: "agent-7".into()
        })
    );
    let Filter::Pred(Predicate::Range { field, min, max }) = &parts[5] else {
        panic!("range expected")
    };
    assert_eq!(*field, Key::Updated);
    assert_eq!(*min, Bound::Included(1_767_225_600_000));
    assert_eq!(*max, Bound::Unbounded);
}
