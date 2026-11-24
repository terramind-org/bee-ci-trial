//! Code search and query languages (PRODUCT §4.4): a trigram index over content-addressed
//! blobs that honours path permissions, plus the forge query syntax.
//!
//! - [`index::Indexer`] keeps a per-`(repo, ref)` manifest current by replaying changeset
//!   deltas, indexing each blob once per org ([`store::IndexStore`], with in-memory and
//!   on-disk implementations).
//! - [`search::Searcher`] runs code queries ([`query::code`]): literals, `/regex/`,
//!   `path:`, `lang:`, `repo:`, `org:`, `symbol:`, `case:yes`, `-`/`NOT`, `AND`/`OR` and
//!   parentheses. Regexes are planned into trigram queries ([`plan`]) and verified on every
//!   candidate. Every file passes the caller's [`search::PathAuthorizer`] before it is read
//!   or counted (SPEC §9.3). It also serves symbol search, definitions, references,
//!   outlines and suggestions ([`symbols`]).
//! - [`forge`] parses and validates GitHub-compatible issue, change request, repo,
//!   changeset, user, topic, discussion and package queries into a typed AST and a
//!   normalized [`forge::Filter`] the forge translates to its own tables.
//! - [`parse_search_query`] backs `ParseSearchQuery`; [`validate_saved_search`] checks a
//!   saved search before the forge stores it.
//!
//! Everything is deterministic: no clock, randomness or hash-map iteration order affects
//! results, ranks or page tokens. The full reference is docs/spec/search.md.

pub mod forge;
pub mod index;
pub mod lang;
pub mod pathindex;
pub mod plan;
pub mod query;
pub mod search;
pub mod store;
pub mod symbols;
pub mod trigram;

pub use forge::{ForgeKind, ForgeQuery, parse_forge_query};
pub use index::{IndexStats, Indexer};
pub use query::code::{CodeQuery, parse_code_query};
pub use query::{QueryError, Span};
pub use search::{CodeSearchRequest, CodeSearchResponse, PathAuthorizer, RepoTarget, Searcher};
pub use store::{DiskIndexStore, IndexStore, MemoryIndexStore};

use bee_core::{BeeError, Result};

/// Which grammar a query uses (mirrors `bee.v1.SearchQueryKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryKind {
    /// Code search.
    Code,
    /// Symbol search (code grammar; bare terms name symbols).
    Symbols,
    /// A forge grammar.
    Forge(ForgeKind),
}

/// One parsed term (mirrors `bee.v1.SearchQualifier`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedQualifier {
    /// Canonical key; empty for free text.
    pub key: String,
    /// Value as written.
    pub value: String,
    /// Under an odd number of negations.
    pub negated: bool,
    /// Written as `/regex/`.
    pub regex: bool,
    /// Position.
    pub span: Span,
    /// Canonical value.
    pub resolved: String,
}

/// The answer to `ParseSearchQuery` (mirrors `bee.v1.ParseSearchQueryResponse`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseOutcome {
    /// No errors.
    pub valid: bool,
    /// Every term, in query order (empty when invalid).
    pub qualifiers: Vec<ParsedQualifier>,
    /// Errors with positions.
    pub errors: Vec<QueryError>,
    /// Canonical form of a valid query.
    pub normalized: String,
}

/// Checks a query without running it.
pub fn parse_search_query(kind: QueryKind, query: &str) -> ParseOutcome {
    let fail = |errors: Vec<QueryError>| ParseOutcome {
        valid: false,
        qualifiers: Vec::new(),
        errors,
        normalized: String::new(),
    };
    match kind {
        QueryKind::Code | QueryKind::Symbols => match parse_code_query(query) {
            Ok(q) => ParseOutcome {
                valid: true,
                qualifiers: q
                    .qualifiers
                    .iter()
                    .map(|i| ParsedQualifier {
                        key: if kind == QueryKind::Symbols && i.key.is_empty() {
                            "symbol".into()
                        } else {
                            i.key.clone()
                        },
                        value: i.value.clone(),
                        negated: i.negated,
                        regex: i.regex,
                        span: i.span,
                        resolved: i.resolved.clone(),
                    })
                    .collect(),
                errors: Vec::new(),
                normalized: q.normalized(),
            },
            Err(e) => fail(e),
        },
        QueryKind::Forge(fk) => match parse_forge_query(fk, query) {
            Ok(q) => ParseOutcome {
                valid: true,
                qualifiers: q
                    .qualifiers
                    .iter()
                    .map(|i| ParsedQualifier {
                        key: i.key.clone(),
                        value: i.value.clone(),
                        negated: i.negated,
                        regex: false,
                        span: i.span,
                        resolved: i.resolved.clone(),
                    })
                    .collect(),
                errors: Vec::new(),
                normalized: q.normalized(),
            },
            Err(e) => fail(e),
        },
    }
}

/// Longest saved-search name, in bytes.
pub const MAX_SAVED_SEARCH_NAME: usize = 100;
/// Longest saved query, in bytes.
pub const MAX_SAVED_SEARCH_QUERY: usize = 1024;

/// Validates a saved search before the forge stores it: a non-blank name of at most
/// [`MAX_SAVED_SEARCH_NAME`] bytes without control characters, and a query that parses for
/// its kind and is at most [`MAX_SAVED_SEARCH_QUERY`] bytes. Returns the normalized query
/// to store. Errors are `INVALID_ARGUMENT` (`bad_query` for the query, `bad_name` for the
/// name) or `TOO_LARGE`.
pub fn validate_saved_search(kind: QueryKind, name: &str, query: &str) -> Result<String> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(BeeError::new(
            bee_core::Reason::InvalidArgument,
            "saved search name must be non-blank text",
        )
        .with_payload("subcode", "bad_name"));
    }
    if name.len() > MAX_SAVED_SEARCH_NAME {
        return Err(BeeError::too_large(
            "saved_search_name_bytes",
            MAX_SAVED_SEARCH_NAME as u64,
            name.len() as u64,
        ));
    }
    if query.len() > MAX_SAVED_SEARCH_QUERY {
        return Err(BeeError::too_large(
            "saved_search_query_bytes",
            MAX_SAVED_SEARCH_QUERY as u64,
            query.len() as u64,
        ));
    }
    let out = parse_search_query(kind, query);
    if !out.valid {
        return Err(query::to_bee_error(&out.errors));
    }
    Ok(out.normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::Reason;

    #[test]
    fn parse_outcomes() {
        let ok = parse_search_query(QueryKind::Code, "foo lang:go");
        assert!(ok.valid);
        assert_eq!(ok.normalized, "foo lang:go");
        let sym = parse_search_query(QueryKind::Symbols, "Invoice");
        assert_eq!(sym.qualifiers[0].key, "symbol");
        let bad = parse_search_query(QueryKind::Forge(ForgeKind::Issues), "is:nope");
        assert!(!bad.valid);
        assert_eq!(bad.errors[0].subcode, "bad_value");
    }

    #[test]
    fn saved_searches() {
        assert_eq!(
            validate_saved_search(
                QueryKind::Forge(ForgeKind::Issues),
                "Mine",
                "IS:open author:@me"
            )
            .unwrap(),
            "is:open author:@me"
        );
        let e = validate_saved_search(QueryKind::Code, " ", "x").unwrap_err();
        assert_eq!(e.reason, Reason::InvalidArgument);
        let e = validate_saved_search(QueryKind::Code, "n", "lang:cobol").unwrap_err();
        assert_eq!(e.reason, Reason::InvalidArgument);
        let e = validate_saved_search(QueryKind::Code, "n", &"x".repeat(2000)).unwrap_err();
        assert_eq!(e.reason, Reason::TooLarge);
    }
}
