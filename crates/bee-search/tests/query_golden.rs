//! Golden tests for every query grammar: the normalized form of valid queries and the error
//! subcodes of invalid ones. Normalized forms must also re-parse to themselves.

use std::ops::Bound;

use bee_search::forge::{
    CivilDate, Filter, FilterContext, ForgeKind, Key, Predicate, parse_forge_query,
};
use bee_search::{QueryKind, parse_search_query};

use ForgeKind::{
    Changesets as C, Discussions as D, Issues as I, Packages as P, Repos as R, Topics as T,
    Users as U,
};

enum Want {
    Ok(&'static str),
    Err(&'static [&'static str]),
}

use Want::{Err, Ok};

fn golden() -> Vec<(QueryKind, &'static str, Want)> {
    let i = QueryKind::Forge(I);
    let code = QueryKind::Code;
    vec![
        // ----- issues and change requests -----
        (
            i,
            "is:open is:cr review:required path:billing/",
            Ok("is:open is:cr review:required path:billing/"),
        ),
        (i, "is:open is:pr", Ok("is:open is:pr")),
        (i, "IS:Closed  label:bug", Ok("is:closed label:bug")),
        (
            i,
            "is:merged base:main head:feature/x",
            Ok("is:merged base:main head:feature/x"),
        ),
        (i, "is:draft author:@me", Ok("is:draft author:@me")),
        (
            i,
            "assignee:octo-cat label:\"good first issue\"",
            Ok("assignee:octo-cat label:\"good first issue\""),
        ),
        (i, "milestone:v1.0 no:label", Ok("milestone:v1.0 no:label")),
        (
            i,
            "review:approved status:success",
            Ok("review:approved status:success"),
        ),
        (
            i,
            "review:changes_requested status:failure",
            Ok("review:changes_requested status:failure"),
        ),
        (i, "crash in:title,body", Ok("crash in:title,body")),
        (
            i,
            "crash in:comments in:title",
            Err(&["duplicate_qualifier"]),
        ),
        (
            i,
            "created:>=2024-01-01 updated:<2024-06-01",
            Ok("created:>=2024-01-01 updated:<2024-06-01"),
        ),
        (
            i,
            "created:2024-01-01..2024-02-01",
            Ok("created:2024-01-01..2024-02-01"),
        ),
        (i, "closed:*..2024-02-01", Ok("closed:*..2024-02-01")),
        (i, "updated:>@today-7d", Ok("updated:>@today-7d")),
        (
            i,
            "merged:2024-03-01T10:00:00+02:00",
            Ok("merged:2024-03-01T08:00:00Z"),
        ),
        (
            i,
            "comments:>10 reactions:5..20",
            Ok("comments:>10 reactions:5..20"),
        ),
        (
            i,
            "involves:alice mentions:bob commenter:carol",
            Ok("involves:alice mentions:bob commenter:carol"),
        ),
        (
            i,
            "reviewed-by:dana review-requested:@me",
            Ok("reviewed-by:dana review-requested:@me"),
        ),
        (
            i,
            "team-review-requested:acme/backend",
            Ok("team-review-requested:acme/backend"),
        ),
        (i, "risk:sensitive", Ok("risk:sensitive")),
        (i, "risk:>=logic is:open", Ok("risk:>=logic is:open")),
        (i, "risk:docs..test_only", Ok("risk:docs..test_only")),
        (i, "agent:true is:cr", Ok("agent:true is:cr")),
        (i, "agent:reviewer-bot", Ok("agent:reviewer-bot")),
        (
            i,
            "-label:wontfix -author:spammer",
            Ok("-label:wontfix -author:spammer"),
        ),
        (i, "NOT label:wontfix", Ok("-label:wontfix")),
        (
            i,
            "label:bug OR label:crash",
            Ok("label:bug OR label:crash"),
        ),
        (
            i,
            "(label:bug OR label:crash) is:open",
            Ok("(label:bug OR label:crash) is:open"),
        ),
        (
            i,
            "is:open (author:a OR assignee:a) sort:created-asc",
            Ok("is:open (author:a OR assignee:a) sort:created-asc"),
        ),
        (i, "sort:comments", Ok("sort:comments-desc")),
        (
            i,
            "repo:acme/api org:acme user:octo",
            Ok("repo:acme/api org:acme user:octo"),
        ),
        (i, "lang:rust is:issue", Ok("language:rust is:issue")),
        (
            i,
            "linked:pr reason:not-planned",
            Ok("linked:pr reason:not-planned"),
        ),
        (
            i,
            "archived:false draft:true",
            Ok("archived:false draft:true"),
        ),
        (i, "is:unmerged is:locked", Ok("is:unmerged is:locked")),
        (
            i,
            "\"memory leak\" type:issue",
            Ok("\"memory leak\" type:issue"),
        ),
        (i, "is:nope", Err(&["bad_value"])),
        (i, "review:maybe", Err(&["bad_value"])),
        (i, "created:yesterday", Err(&["bad_date"])),
        (i, "created:2024-13-01", Err(&["bad_date"])),
        (i, "comments:lots", Err(&["bad_value"])),
        (i, "colour:red", Err(&["unknown_qualifier"])),
        (i, "stars:>10", Err(&["wrong_kind"])),
        (i, "(is:open", Err(&["unbalanced_paren"])),
        (i, "is:open OR", Err(&["dangling_operator"])),
        (i, "label:\"unterminated", Err(&["unterminated_quote"])),
        (i, "-sort:created", Err(&["misplaced_qualifier"])),
        (
            i,
            "sort:created sort:updated",
            Err(&["duplicate_qualifier"]),
        ),
        (i, "sort:stars", Err(&["bad_value"])),
        (i, "risk:catastrophic", Err(&["bad_value"])),
        (i, "author:@someone", Err(&["bad_value"])),
        (i, "in:title,nowhere", Err(&["bad_value"])),
        (i, "error: something broke", Ok("error: something broke")),
        // ----- repos -----
        (
            QueryKind::Forge(R),
            "topic:rust language:go stars:>=100",
            Ok("topic:rust language:go stars:>=100"),
        ),
        (
            QueryKind::Forge(R),
            "is:public archived:false fork:only",
            Ok("is:public archived:false fork:only"),
        ),
        (
            QueryKind::Forge(R),
            "pushed:>2024-01-01 forks:10..* sort:stars",
            Ok("pushed:>2024-01-01 forks:10..* sort:stars-desc"),
        ),
        (
            QueryKind::Forge(R),
            "cli in:name,description license:mit",
            Ok("cli license:mit in:name,description"),
        ),
        (
            QueryKind::Forge(R),
            "good-first-issues:>2 help-wanted-issues:>=1",
            Ok("good-first-issues:>2 help-wanted-issues:>=1"),
        ),
        (QueryKind::Forge(R), "review:approved", Err(&["wrong_kind"])),
        // ----- changesets -----
        (
            QueryKind::Forge(C),
            "fix author:alice committer-date:>2024-01-01",
            Ok("fix author:alice committer-date:>2024-01-01"),
        ),
        (
            QueryKind::Forge(C),
            "merge:false hash:b1_abc parent:b1_def",
            Ok("merge:false hash:b1_abc parent:b1_def"),
        ),
        (
            QueryKind::Forge(C),
            "author-email:a@x.io change-id:zzz agent:true",
            Ok("author-email:a@x.io change-id:zzz agent:true"),
        ),
        (
            QueryKind::Forge(C),
            "sort:author-date-asc repo:acme/api",
            Ok("repo:acme/api sort:author-date-asc"),
        ),
        // ----- users -----
        (
            QueryKind::Forge(U),
            "type:agent followers:>10 location:berlin",
            Ok("type:agent followers:>10 location:berlin"),
        ),
        (
            QueryKind::Forge(U),
            "ada in:login,name sort:joined-asc",
            Ok("ada in:login,name sort:joined-asc"),
        ),
        (QueryKind::Forge(U), "type:robot", Err(&["bad_value"])),
        // ----- topics, discussions, packages -----
        (
            QueryKind::Forge(T),
            "is:featured repositories:>100",
            Ok("is:featured repositories:>100"),
        ),
        (
            QueryKind::Forge(D),
            "category:q-a is:answered author:@me",
            Ok("category:q-a is:answered author:@me"),
        ),
        (QueryKind::Forge(D), "is:merged", Err(&["bad_value"])),
        (
            QueryKind::Forge(P),
            "type:npm org:acme sort:downloads",
            Ok("type:npm org:acme sort:downloads-desc"),
        ),
        (QueryKind::Forge(P), "type:apt", Err(&["bad_value"])),
        // ----- code -----
        (code, "charge_card", Ok("charge_card")),
        (
            code,
            "/\\bcharge_card\\(/ org:acme",
            Ok("/\\bcharge_card\\(/ org:acme"),
        ),
        (
            code,
            "foo path:src/ language:Rust",
            Ok("foo path:src/ lang:rust"),
        ),
        (
            code,
            "foo file:*.go -path:vendor/**",
            Ok("foo path:*.go -path:vendor/**"),
        ),
        (
            code,
            "symbol:Invoice repo:acme/billing",
            Ok("symbol:Invoice repo:acme/billing"),
        ),
        (code, "\"fn main\" case:yes", Ok("\"fn main\" case:yes")),
        (code, "foo OR bar case:yes", Ok("foo OR bar case:yes")),
        (code, "std::io::Read", Ok("std::io::Read")),
        (code, "content:lang:x", Ok("content:lang:x")),
        (code, "foo( bar()", Ok("foo( \"bar()\"")),
        (code, "(foo OR bar) NOT baz", Ok("(foo OR bar) -baz")),
        (code, "lang:cobol", Err(&["unknown_language"])),
        (code, "/(unclosed/", Err(&["bad_regex"])),
        (code, "case:yes", Err(&["empty_query"])),
        (code, "", Err(&["empty_query"])),
        (code, "foo -case:yes", Err(&["misplaced_qualifier"])),
        (code, "foo (bar", Err(&["unbalanced_paren"])),
        (code, "path:[ab foo", Err(&["bad_value"])),
        (code, "foo AND", Err(&["dangling_operator"])),
        (
            QueryKind::Symbols,
            "Invoice lang:rust",
            Ok("Invoice lang:rust"),
        ),
    ]
}

#[test]
fn query_parser_golden() {
    let cases = golden();
    assert!(cases.len() >= 60, "only {} golden queries", cases.len());
    for (kind, q, want) in cases {
        let out = parse_search_query(kind, q);
        match want {
            Ok(norm) => {
                assert!(out.valid, "{kind:?} {q:?}: {:?}", out.errors);
                assert_eq!(out.normalized, norm, "{kind:?} {q:?}");
                let again = parse_search_query(kind, &out.normalized);
                assert!(
                    again.valid,
                    "{kind:?} {norm:?} does not re-parse: {:?}",
                    again.errors
                );
                assert_eq!(
                    again.normalized, norm,
                    "{kind:?} {norm:?} is not a fixed point"
                );
            }
            Err(codes) => {
                assert!(!out.valid, "{kind:?} {q:?} should fail");
                let got: Vec<&str> = out.errors.iter().map(|e| e.subcode).collect();
                assert_eq!(got, codes, "{kind:?} {q:?}");
                for e in &out.errors {
                    assert!(e.span.end as usize <= q.len() && e.span.start <= e.span.end);
                }
            }
        }
    }
}

#[test]
fn qualifier_positions_and_negation() {
    let out = parse_search_query(QueryKind::Forge(I), "is:open -label:bug sort:updated");
    let q: Vec<(&str, &str, bool, u32, u32)> = out
        .qualifiers
        .iter()
        .map(|q| {
            (
                q.key.as_str(),
                q.value.as_str(),
                q.negated,
                q.span.start,
                q.span.end,
            )
        })
        .collect();
    assert_eq!(
        q,
        vec![
            ("is", "open", false, 0, 7),
            ("label", "bug", true, 8, 18),
            ("sort", "updated", false, 19, 31),
        ]
    );
    assert_eq!(out.qualifiers[2].resolved, "updated-desc");
}

#[test]
fn filters_resolve_shorthands_dates_and_text() {
    let ctx = FilterContext {
        me: Some("me-bot".into()),
        today: CivilDate::new(2024, 3, 10),
    };
    let f = parse_forge_query(
        I,
        "crash in:title is:unmerged no:assignee author:@me created:@today comments:>=3 risk:<=docs",
    )
    .unwrap()
    .to_filter(&ctx)
    .unwrap();
    let Filter::And(parts) = f.filter else {
        panic!()
    };
    assert_eq!(
        parts[0],
        Filter::Pred(Predicate::Text {
            text: "crash".into(),
            fields: vec!["title"]
        })
    );
    assert_eq!(
        parts[1],
        Filter::And(vec![
            Filter::Pred(Predicate::Enum {
                field: "type",
                value: "cr"
            }),
            Filter::Not(Box::new(Filter::Pred(Predicate::Enum {
                field: "state",
                value: "merged"
            }))),
        ])
    );
    assert_eq!(parts[2], Filter::Pred(Predicate::Missing("assignee")));
    assert_eq!(
        parts[3],
        Filter::Pred(Predicate::Person {
            role: Key::Author,
            login: "me-bot".into()
        })
    );
    let day = CivilDate::new(2024, 3, 10).unwrap().days_since_epoch() * 86_400_000;
    assert_eq!(
        parts[4],
        Filter::Pred(Predicate::Range {
            field: Key::Created,
            min: Bound::Included(day),
            max: Bound::Excluded(day + 86_400_000)
        })
    );
    assert_eq!(
        parts[5],
        Filter::Pred(Predicate::Range {
            field: Key::Comments,
            min: Bound::Included(3),
            max: Bound::Unbounded
        })
    );
    assert_eq!(
        parts[6],
        Filter::Pred(Predicate::Range {
            field: Key::Risk,
            min: Bound::Unbounded,
            max: Bound::Included(1)
        })
    );
    assert_eq!(f.sort.field, "best-match");

    // Default text fields per kind, agent filters, and an empty query matching all.
    let f = parse_forge_query(C, "fix agent:true")
        .unwrap()
        .to_filter(&ctx)
        .unwrap();
    assert_eq!(
        f.filter,
        Filter::And(vec![
            Filter::Pred(Predicate::Text {
                text: "fix".into(),
                fields: vec!["message"]
            }),
            Filter::Pred(Predicate::AgentAuthor {
                is_agent: true,
                login: None
            }),
        ])
    );
    let f = parse_forge_query(R, "sort:stars-asc")
        .unwrap()
        .to_filter(&ctx)
        .unwrap();
    assert_eq!(f.filter, Filter::All);
    assert_eq!(f.sort.to_string(), "stars-asc");
}
