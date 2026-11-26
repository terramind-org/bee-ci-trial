//! Executing code queries: candidate selection from posting lists, authorization,
//! verification, ranking, facets, pagination, symbols, references and suggestions.
//!
//! # Authorization (SPEC §9.3)
//!
//! Every file is checked with the caller's [`PathAuthorizer`] before its content is read,
//! matched, counted or ranked. A file the caller cannot read therefore never contributes to
//! hits, `total_count`, facets, symbol results, references or suggestions, and dedup never
//! leaks content: documents are shared per org, but a hit is always reported through a
//! `(repo, path)` the caller can read.
//!
//! # Ranking and pagination
//!
//! A file's rank is its best match kind (exact text of a query literal on word boundaries >
//! any whole-word match > exact text inside a longer word > other match; symbol definitions
//! count as exact), then shallower paths.
//! Ties break by repo full name, ref and path, so the order is total and deterministic. A
//! page token encodes the sort key of the last hit returned plus a fingerprint of the
//! query; the next page starts strictly after that key, so pages never repeat or skip a hit
//! that existed on both requests.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;

use bee_core::{BeeError, ObjectId, Reason, RepoPath, Result, hex};
use bee_store::{OrgId, RepoId};
use regex::bytes::{Regex, RegexBuilder};

use crate::lang::Language;
use crate::pathindex::PathIndex;
use crate::plan::{TrigramQuery, plan_regex};
use crate::query::code::{
    CodeQuery, CodeTerm, Pattern, REGEX_SIZE_LIMIT, RepoPattern, parse_code_query,
};
use crate::query::{Expr, to_bee_error};
use crate::store::codec::{Dec, Enc};
use crate::store::{DocNum, Document, FileEntry, IndexStore, Manifest};
use crate::symbols::{self, Symbol, SymbolKind};

/// Decides whether the caller may read a path in a repo (SPEC §9.3).
pub trait PathAuthorizer {
    /// True when the caller may read `path` through `repo`.
    fn can_read(&self, repo: RepoId, path: &RepoPath) -> bool;
}

impl<F: Fn(RepoId, &RepoPath) -> bool> PathAuthorizer for F {
    fn can_read(&self, repo: RepoId, path: &RepoPath) -> bool {
        self(repo, path)
    }
}

/// A `(repo, ref)` to search, with the names `repo:` and `org:` match against. The caller
/// passes only repos it may see at all; paths inside are checked by the authorizer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoTarget {
    /// The org.
    pub org: OrgId,
    /// The org's name.
    pub org_name: String,
    /// The repo.
    pub repo: RepoId,
    /// The repo's name within the org.
    pub repo_name: String,
    /// The indexed ref, e.g. `heads/main`.
    pub ref_name: String,
}

impl RepoTarget {
    /// `org/repo`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.org_name, self.repo_name)
    }
}

/// Limits and defaults of the searcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchLimits {
    /// Page size when the request gives 0.
    pub default_page_size: u32,
    /// Largest page size honoured.
    pub max_page_size: u32,
    /// Line matches per file when the request gives 0.
    pub default_line_matches: u32,
    /// Longest line text returned, in bytes (cut on a character boundary).
    pub max_line_bytes: usize,
    /// Most context lines honoured.
    pub max_context_lines: u32,
    /// Matches counted per file before counting stops.
    pub max_matches_per_file: usize,
}

impl Default for SearchLimits {
    fn default() -> SearchLimits {
        SearchLimits {
            default_page_size: 25,
            max_page_size: 100,
            default_line_matches: 20,
            max_line_bytes: 500,
            max_context_lines: 10,
            max_matches_per_file: 10_000,
        }
    }
}

/// A code search request (mirrors `bee.v1.SearchCodeRequest`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeSearchRequest {
    /// The query.
    pub query: String,
    /// Force case-sensitive matching (as `case:yes`).
    pub case_sensitive: bool,
    /// Context lines around each match (at most [`SearchLimits::max_context_lines`]).
    pub context_lines: u32,
    /// Line matches per file; 0 = default.
    pub max_line_matches_per_file: u32,
    /// Compute language and repo facets.
    pub include_facets: bool,
    /// Page size; 0 = default.
    pub page_size: u32,
    /// Token from a previous response; empty for the first page.
    pub page_token: String,
}

/// A byte range within a line, half-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineRange {
    /// Start byte.
    pub start: u32,
    /// End byte.
    pub end: u32,
}

/// One matching line (mirrors `bee.v1.CodeSearchLineMatch`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    /// 1-based line number.
    pub line_number: u32,
    /// 0-based byte column of the first match on the line.
    pub column: u32,
    /// The line text (UTF-8, possibly cut).
    pub line: String,
    /// Match ranges within `line`.
    pub ranges: Vec<LineRange>,
    /// Lines before.
    pub context_before: Vec<String>,
    /// Lines after.
    pub context_after: Vec<String>,
}

/// One file with matches (mirrors `bee.v1.CodeSearchHit`).
#[derive(Clone, Debug, PartialEq)]
pub struct CodeHit {
    /// The repo.
    pub repo: RepoId,
    /// `org/repo`.
    pub repo_full_name: String,
    /// The path.
    pub path: RepoPath,
    /// The indexed ref.
    pub ref_name: String,
    /// The indexed changeset.
    pub changeset: ObjectId,
    /// The blob.
    pub blob: ObjectId,
    /// Detected language.
    pub language: Option<Language>,
    /// Matching lines, at most the per-file limit.
    pub line_matches: Vec<LineMatch>,
    /// Total matches in the file (may exceed `line_matches`).
    pub match_count: u32,
    /// A `path:` condition matched.
    pub path_matched: bool,
    /// Matched symbol definitions (for `symbol:` terms).
    pub symbols: Vec<Symbol>,
    /// Rank score; higher first.
    pub score: f32,
}

/// Counts per value of one facet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facet {
    /// `language` or `repo`.
    pub name: String,
    /// `(value, count)`, by count descending then value.
    pub values: Vec<(String, u64)>,
}

/// A page of code search results (mirrors `bee.v1.SearchCodeResponse`).
#[derive(Clone, Debug, PartialEq)]
pub struct CodeSearchResponse {
    /// The hits of this page.
    pub hits: Vec<CodeHit>,
    /// Matching files the caller can read, over all pages.
    pub total_count: u64,
    /// Some targets are not indexed yet.
    pub incomplete: bool,
    /// The targets that are not indexed yet, as `org/repo@ref`.
    pub skipped_repos: Vec<String>,
    /// Facets, when requested.
    pub facets: Vec<Facet>,
    /// Token for the next page; `None` on the last page.
    pub next_page_token: Option<String>,
}

/// Summary of a streamed search (mirrors `bee.v1.CodeSearchStreamDone`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSummary {
    /// Files streamed.
    pub files_matched: u64,
    /// Matches in the streamed files.
    pub matches: u64,
    /// Stopped at the match limit.
    pub limit_hit: bool,
    /// Targets not indexed yet.
    pub skipped_repos: Vec<String>,
    /// Facets over the streamed files.
    pub facets: Vec<Facet>,
}

/// Where a symbol is defined or referenced (mirrors `bee.v1.SymbolLocation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolLocation {
    /// The repo.
    pub repo: RepoId,
    /// `org/repo`.
    pub repo_full_name: String,
    /// The ref.
    pub ref_name: String,
    /// The changeset.
    pub changeset: ObjectId,
    /// The path.
    pub path: RepoPath,
    /// 1-based line.
    pub line_number: u32,
    /// 0-based start column (bytes).
    pub column: u32,
    /// End column (bytes, exclusive).
    pub end_column: u32,
    /// The source line.
    pub line: String,
    /// The definition, when this location is one.
    pub symbol: Option<Symbol>,
    /// Language of the file.
    pub language: Option<Language>,
}

/// A page of symbol results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolSearchResponse {
    /// Definitions of this page.
    pub symbols: Vec<SymbolLocation>,
    /// Definitions over all pages.
    pub total_count: u64,
    /// Some targets are not indexed yet.
    pub incomplete: bool,
    /// Token for the next page.
    pub next_page_token: Option<String>,
}

/// What a suggestion completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SuggestionKind {
    /// A repo full name.
    Repo,
    /// A file path.
    Path,
    /// A symbol name.
    Symbol,
}

/// One type-ahead suggestion.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Suggestion {
    /// What it is.
    pub kind: SuggestionKind,
    /// The text to insert.
    pub text: String,
    /// `org/repo` it comes from (empty for repos).
    pub repo_full_name: String,
}

/// A compiled condition.
enum Cond {
    Content { re: Regex, literal: Option<Vec<u8>> },
    Symbol(Regex),
    Path(Regex),
    Lang(Language),
    Repo(RepoPattern),
    Org(String),
}

/// A query ready to run.
struct Compiled {
    expr: Expr<Cond>,
    plan: TrigramQuery,
    /// The `path:` terms planned for the path index (`crate::pathindex`).
    path_plan: TrigramQuery,
    fingerprint: [u8; 8],
    has_symbol_terms: bool,
}

fn compile_err(e: &regex::Error) -> BeeError {
    match e {
        regex::Error::CompiledTooBig(limit) => BeeError::new(
            Reason::TooLarge,
            format!("regex compiles to more than {limit} bytes"),
        )
        .with_payload("limit", "regex_size"),
        other => BeeError::new(Reason::InvalidArgument, other.to_string())
            .with_payload("subcode", crate::query::BAD_QUERY),
    }
}

fn plan_pattern(p: &Pattern, case_sensitive: bool) -> TrigramQuery {
    // Validated at parse time; a planning failure only loses selectivity.
    plan_regex(&p.regex_source(), !case_sensitive).unwrap_or(TrigramQuery::All)
}

fn plan_expr(e: &Expr<CodeTerm>, case_sensitive: bool) -> TrigramQuery {
    match e {
        Expr::Term(CodeTerm::Content(p) | CodeTerm::Symbol(p)) => plan_pattern(p, case_sensitive),
        Expr::Term(_) | Expr::Not(_) => TrigramQuery::All,
        Expr::And(v) => TrigramQuery::and(v.iter().map(|c| plan_expr(c, case_sensitive)).collect()),
        Expr::Or(v) => TrigramQuery::or(v.iter().map(|c| plan_expr(c, case_sensitive)).collect()),
    }
}

/// Plans the `path:` terms of a query for the path index; other terms plan to "all".
fn plan_path_expr(e: &Expr<CodeTerm>) -> TrigramQuery {
    match e {
        // Folded trigrams: planned case-insensitively whatever the query's case flag.
        Expr::Term(CodeTerm::Path(p)) => {
            plan_regex(&p.regex_source(), true).unwrap_or(TrigramQuery::All)
        }
        Expr::Term(_) | Expr::Not(_) => TrigramQuery::All,
        Expr::And(v) => TrigramQuery::and(v.iter().map(plan_path_expr).collect()),
        Expr::Or(v) => TrigramQuery::or(v.iter().map(plan_path_expr).collect()),
    }
}

fn compile(q: &CodeQuery, force_case: bool) -> Result<Compiled> {
    let cs = q.case_sensitive || force_case;
    let plan = plan_expr(&q.expr, cs);
    let path_plan = plan_path_expr(&q.expr);
    let mut has_symbol_terms = false;
    let mut errors: Vec<BeeError> = Vec::new();
    let expr = q.expr.clone().try_map(
        &mut |t: CodeTerm, _neg: bool| -> std::result::Result<Cond, BeeError> {
            Ok(match t {
                CodeTerm::Content(p) => Cond::Content {
                    re: p.compile(cs).map_err(|e| compile_err(&e))?,
                    literal: match &p {
                        Pattern::Literal(s) => Some(s.as_bytes().to_vec()),
                        Pattern::Regex(_) => None,
                    },
                },
                CodeTerm::Symbol(p) => {
                    has_symbol_terms = true;
                    let src = match &p {
                        Pattern::Literal(s) => format!("^(?:{})$", regex::escape(s)),
                        Pattern::Regex(r) => r.clone(),
                    };
                    Cond::Symbol(
                        RegexBuilder::new(&src)
                            .case_insensitive(!cs)
                            .size_limit(REGEX_SIZE_LIMIT)
                            .build()
                            .map_err(|e| compile_err(&e))?,
                    )
                }
                CodeTerm::Path(p) => Cond::Path(p.compile(cs).map_err(|e| compile_err(&e))?),
                CodeTerm::Lang(l) => Cond::Lang(l),
                CodeTerm::Repo(r) => Cond::Repo(r),
                CodeTerm::Org(o) => Cond::Org(o),
            })
        },
        &mut errors,
    );
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    let expr =
        expr.ok_or_else(|| BeeError::new(Reason::Internal, "query compilation lost terms"))?;
    let mut fp_src = q.normalized().into_bytes();
    fp_src.push(u8::from(cs));
    let d = bee_object::digest(&fp_src);
    let mut fingerprint = [0u8; 8];
    fingerprint.copy_from_slice(&d[..8]);
    Ok(Compiled {
        expr,
        plan,
        path_plan,
        fingerprint,
        has_symbol_terms,
    })
}

/// Sort key: rank descending, then repo, ref, path, line, column ascending.
type SortKey = (Reverse<i64>, String, String, Vec<u8>, u32, u32);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cursor {
    fingerprint: [u8; 8],
    rank: i64,
    repo: String,
    ref_name: String,
    path: Vec<u8>,
    line: u32,
    column: u32,
}

impl Cursor {
    fn key(&self) -> SortKey {
        (
            Reverse(self.rank),
            self.repo.clone(),
            self.ref_name.clone(),
            self.path.clone(),
            self.line,
            self.column,
        )
    }

    fn encode(&self) -> String {
        let mut e = Enc::default();
        e.u8(1);
        e.raw(&self.fingerprint);
        e.u64(self.rank as u64);
        e.bytes(self.repo.as_bytes());
        e.bytes(self.ref_name.as_bytes());
        e.bytes(&self.path);
        e.u32(self.line);
        e.u32(self.column);
        hex::encode(&e.buf)
    }

    fn decode(token: &str, fingerprint: [u8; 8]) -> Result<Cursor> {
        let bad = || {
            BeeError::new(
                Reason::InvalidArgument,
                "page token is malformed or belongs to another query",
            )
            .with_payload("subcode", "bad_page_token")
        };
        let bytes = hex::decode(token).ok_or_else(bad)?;
        let mut d = Dec::new(&bytes);
        let parse = |d: &mut Dec<'_>| -> Result<Cursor> {
            if d.u8()? != 1 {
                return Err(bad());
            }
            let mut fp = [0u8; 8];
            fp.copy_from_slice(d.raw(8)?);
            let rank = d.u64()? as i64;
            let repo = String::from_utf8(d.bytes()?.to_vec()).map_err(|_| bad())?;
            let ref_name = String::from_utf8(d.bytes()?.to_vec()).map_err(|_| bad())?;
            let path = d.bytes()?.to_vec();
            let line = d.u32()?;
            let column = d.u32()?;
            Ok(Cursor {
                fingerprint: fp,
                rank,
                repo,
                ref_name,
                path,
                line,
                column,
            })
        };
        let c = parse(&mut d).map_err(|_| bad())?;
        if !d.done() || c.fingerprint != fingerprint {
            return Err(bad());
        }
        Ok(c)
    }
}

/// A verified file match before shaping into a hit.
struct FileMatch {
    target: usize,
    manifest: Arc<Manifest>,
    path: RepoPath,
    entry: FileEntry,
    doc: Option<Arc<Document>>,
    ranges: Vec<(usize, usize, u8)>,
    symbols: Vec<Symbol>,
    path_matched: bool,
    rank: i64,
}

/// Per-file evaluation context.
struct FileCtx<'a> {
    store: &'a dyn IndexStore,
    org: OrgId,
    target: &'a RepoTarget,
    path: &'a RepoPath,
    entry: &'a FileEntry,
    doc: Option<Option<Arc<Document>>>,
    symbols: Option<Vec<Symbol>>,
    max_matches: usize,
}

#[derive(Default)]
struct Acc {
    ranges: Vec<(usize, usize, u8)>,
    symbols: Vec<Symbol>,
    path_matched: bool,
}

impl Acc {
    fn merge(&mut self, other: Acc) {
        self.ranges.extend(other.ranges);
        self.symbols.extend(other.symbols);
        self.path_matched |= other.path_matched;
    }
}

/// Match kinds, best first: the exact text of a query literal bounded by word boundaries;
/// a case-folded whole word (or any regex match bounded by word boundaries); the exact text
/// inside a longer word; any other match.
const KIND_EXACT: u8 = 4;
const KIND_WORD: u8 = 3;
const KIND_CASE_SUBSTRING: u8 = 2;
const KIND_SUBSTRING: u8 = 1;

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

impl FileCtx<'_> {
    fn doc(&mut self) -> Result<Option<Arc<Document>>> {
        if self.doc.is_none() {
            let d = match self.entry.doc {
                Some(n) => self.store.document(self.org, n)?,
                None => None,
            };
            self.doc = Some(d.filter(|d| d.skip.is_none()));
        }
        Ok(self.doc.clone().flatten())
    }

    fn symbols(&mut self) -> Result<Vec<Symbol>> {
        if self.symbols.is_none() {
            let s = match (self.doc()?, self.entry.language) {
                (Some(d), Some(l)) => symbols::extract(&d.content, l),
                _ => Vec::new(),
            };
            self.symbols = Some(s);
        }
        Ok(self.symbols.clone().unwrap_or_default())
    }

    fn eval(&mut self, e: &Expr<Cond>, positive: bool, acc: &mut Acc) -> Result<bool> {
        match e {
            Expr::Term(c) => self.term(c, positive, acc),
            Expr::Not(inner) => {
                let mut scratch = Acc::default();
                Ok(!self.eval(inner, !positive, &mut scratch)?)
            }
            Expr::And(v) => {
                let mut local = Acc::default();
                for c in v {
                    if !self.eval(c, positive, &mut local)? {
                        return Ok(false);
                    }
                }
                acc.merge(local);
                Ok(true)
            }
            Expr::Or(v) => {
                let mut any = false;
                for c in v {
                    let mut local = Acc::default();
                    if self.eval(c, positive, &mut local)? {
                        any = true;
                        acc.merge(local);
                        if !positive {
                            break;
                        }
                    }
                }
                Ok(any)
            }
        }
    }

    fn term(&mut self, c: &Cond, positive: bool, acc: &mut Acc) -> Result<bool> {
        Ok(match c {
            Cond::Content { re, literal } => {
                let Some(doc) = self.doc()? else {
                    return Ok(false);
                };
                let content = &doc.content;
                if !positive {
                    return Ok(re.is_match(content));
                }
                let mut found = false;
                for m in re.find_iter(content).take(self.max_matches) {
                    found = true;
                    let (s, e) = (m.start(), m.end());
                    let exact = literal.as_deref().is_some_and(|l| &content[s..e] == l);
                    let bounded = (s == 0 || !is_word(content[s - 1]))
                        && (e >= content.len() || !is_word(content[e]))
                        && e > s;
                    let kind = match (exact, bounded) {
                        (true, true) => KIND_EXACT,
                        (false, true) => KIND_WORD,
                        (true, false) => KIND_CASE_SUBSTRING,
                        (false, false) => KIND_SUBSTRING,
                    };
                    acc.ranges.push((s, e, kind));
                }
                found
            }
            Cond::Symbol(re) => {
                let syms = self.symbols()?;
                let mut found = false;
                for s in syms {
                    if re.is_match(s.name.as_bytes()) {
                        found = true;
                        if positive {
                            acc.symbols.push(s);
                        }
                    }
                }
                found
            }
            Cond::Path(re) => {
                let m = re.is_match(self.path.as_bytes());
                if m && positive {
                    acc.path_matched = true;
                }
                m
            }
            Cond::Lang(l) => self.entry.language == Some(*l),
            Cond::Repo(r) => r.matches(&self.target.org_name, &self.target.repo_name),
            Cond::Org(o) => o.eq_ignore_ascii_case(&self.target.org_name),
        })
    }
}

fn intersect(a: &[DocNum], b: &[DocNum]) -> Vec<DocNum> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
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

fn union(a: &[DocNum], b: &[DocNum]) -> Vec<DocNum> {
    let mut out: Vec<DocNum> = a.iter().chain(b.iter()).copied().collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Candidate documents of an org for a plan; `None` means every document.
fn candidates(store: &dyn IndexStore, org: OrgId, q: &TrigramQuery) -> Result<Option<Vec<DocNum>>> {
    Ok(match q {
        TrigramQuery::All => None,
        TrigramQuery::None => Some(Vec::new()),
        TrigramQuery::Tri(t) => Some(store.postings(org, *t)?),
        TrigramQuery::And(v) => {
            let mut acc: Option<Vec<DocNum>> = None;
            for c in v {
                if let Some(set) = candidates(store, org, c)? {
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
                match candidates(store, org, c)? {
                    None => return Ok(None),
                    Some(set) => acc = union(&acc, &set),
                }
            }
            Some(acc)
        }
    })
}

fn line_starts(content: &[u8]) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(
        content
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .map(|(i, _)| i + 1),
    );
    v
}

fn line_text(content: &[u8], starts: &[usize], idx: usize, max: usize) -> String {
    let s = starts[idx];
    let mut e = starts.get(idx + 1).map_or(content.len(), |n| n - 1);
    if e > s && content[e - 1] == b'\r' {
        e -= 1;
    }
    let raw = &content[s..e.max(s)];
    let mut text = String::from_utf8_lossy(raw).into_owned();
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// Path indexes kept at most (one per manifest searched recently).
const PATH_INDEX_CACHE: usize = 256;

type PathIndexKey = (OrgId, RepoId, String, ObjectId);

/// Runs code queries against an [`IndexStore`].
#[derive(Clone)]
pub struct Searcher {
    store: Arc<dyn IndexStore>,
    limits: SearchLimits,
    path_indexes: Arc<std::sync::Mutex<BTreeMap<PathIndexKey, Arc<PathIndex>>>>,
}

impl Searcher {
    /// A searcher with default limits.
    pub fn new(store: Arc<dyn IndexStore>) -> Searcher {
        Searcher {
            store,
            limits: SearchLimits::default(),
            path_indexes: Arc::default(),
        }
    }

    /// The path index of `manifest`, built on first use and kept for later queries.
    pub fn path_index(&self, manifest: &Manifest) -> Arc<PathIndex> {
        let key = (
            manifest.org,
            manifest.repo,
            manifest.ref_name.clone(),
            manifest.changeset,
        );
        let mut cache = self.path_indexes.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = cache.get(&key) {
            return i.clone();
        }
        if cache.len() >= PATH_INDEX_CACHE {
            cache.clear();
        }
        let built = Arc::new(PathIndex::build(manifest));
        cache.insert(key, built.clone());
        built
    }

    /// Overrides the limits.
    pub fn with_limits(mut self, limits: SearchLimits) -> Searcher {
        self.limits = limits;
        self
    }

    fn parse(query: &str) -> Result<CodeQuery> {
        parse_code_query(query).map_err(|e| to_bee_error(&e))
    }

    /// Finds every authorized matching file. Returns matches and skipped targets.
    fn run(
        &self,
        compiled: &Compiled,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
    ) -> Result<(Vec<FileMatch>, Vec<String>)> {
        let mut out = Vec::new();
        let mut skipped = Vec::new();
        let mut cands: BTreeMap<OrgId, Option<Vec<DocNum>>> = BTreeMap::new();
        for (ti, t) in targets.iter().enumerate() {
            let Some(manifest) = self.store.manifest(t.org, t.repo, &t.ref_name)? else {
                skipped.push(format!("{}@{}", t.full_name(), t.ref_name));
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(e) = cands.entry(t.org) {
                e.insert(candidates(self.store.as_ref(), t.org, &compiled.plan)?);
            }
            let cand = cands.get(&t.org).and_then(Option::as_ref);
            // `path:` terms narrow the files through the path index; every candidate is
            // still verified below.
            let path_index;
            let narrowed: Vec<(&RepoPath, &FileEntry)> = match &compiled.path_plan {
                TrigramQuery::All => manifest.files.iter().collect(),
                plan => {
                    path_index = self.path_index(&manifest);
                    match path_index.candidates(plan) {
                        None => manifest.files.iter().collect(),
                        Some(c) => c
                            .iter()
                            .filter_map(|n| manifest.files.get_key_value(path_index.path(*n)))
                            .collect(),
                    }
                }
            };
            for (path, entry) in narrowed {
                if let Some(c) = cand {
                    match entry.doc {
                        Some(d) if c.binary_search(&d).is_ok() => {}
                        _ => continue,
                    }
                }
                // Authorization before anything about the file is read or counted.
                if !auth.can_read(t.repo, path) {
                    continue;
                }
                let mut ctx = FileCtx {
                    store: self.store.as_ref(),
                    org: t.org,
                    target: t,
                    path,
                    entry,
                    doc: None,
                    symbols: None,
                    max_matches: self.limits.max_matches_per_file,
                };
                let mut acc = Acc::default();
                if !ctx.eval(&compiled.expr, true, &mut acc)? {
                    continue;
                }
                acc.ranges.sort_unstable();
                acc.ranges.dedup_by(|b, a| a.0 == b.0 && a.1 == b.1);
                acc.symbols.sort();
                acc.symbols.dedup();
                let best = acc
                    .ranges
                    .iter()
                    .map(|r| r.2)
                    .chain(acc.symbols.iter().map(|_| KIND_EXACT))
                    .max()
                    .unwrap_or(0);
                let depth = path.depth().min(999) as i64;
                let rank = i64::from(best) * 1000 - depth;
                let doc = ctx.doc.clone().flatten();
                out.push(FileMatch {
                    target: ti,
                    manifest: manifest.clone(),
                    path: path.clone(),
                    entry: entry.clone(),
                    doc,
                    ranges: acc.ranges,
                    symbols: acc.symbols,
                    path_matched: acc.path_matched,
                    rank,
                });
            }
        }
        out.sort_by(|a, b| Self::file_key(a, targets).cmp(&Self::file_key(b, targets)));
        Ok((out, skipped))
    }

    fn file_key(m: &FileMatch, targets: &[RepoTarget]) -> SortKey {
        let t = &targets[m.target];
        (
            Reverse(m.rank),
            t.full_name(),
            t.ref_name.clone(),
            m.path.as_bytes().to_vec(),
            0,
            0,
        )
    }

    fn facets(matches: &[&FileMatch], targets: &[RepoTarget]) -> Vec<Facet> {
        let mut langs: BTreeMap<String, u64> = BTreeMap::new();
        let mut repos: BTreeMap<String, u64> = BTreeMap::new();
        for m in matches {
            let l = m.entry.language.map_or("unknown", Language::name);
            *langs.entry(l.to_string()).or_default() += 1;
            *repos.entry(targets[m.target].full_name()).or_default() += 1;
        }
        let order = |map: BTreeMap<String, u64>| {
            let mut v: Vec<(String, u64)> = map.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v
        };
        vec![
            Facet {
                name: "language".into(),
                values: order(langs),
            },
            Facet {
                name: "repo".into(),
                values: order(repos),
            },
        ]
    }

    fn shape(
        &self,
        m: &FileMatch,
        targets: &[RepoTarget],
        context: u32,
        max_lines: u32,
    ) -> CodeHit {
        let t = &targets[m.target];
        let mut line_matches: Vec<LineMatch> = Vec::new();
        if let Some(doc) = &m.doc {
            let content = &doc.content;
            let starts = line_starts(content);
            let context = context.min(self.limits.max_context_lines) as usize;
            let mut by_line: BTreeMap<usize, Vec<LineRange>> = BTreeMap::new();
            for (s, e, _) in &m.ranges {
                let idx = starts.partition_point(|x| *x <= *s) - 1;
                let line_end = starts.get(idx + 1).map_or(content.len(), |n| n - 1);
                let start = s - starts[idx];
                let end = (*e).min(line_end).max(*s) - starts[idx];
                by_line.entry(idx).or_default().push(LineRange {
                    start: start as u32,
                    end: end as u32,
                });
            }
            for s in &m.symbols {
                let idx = s.line as usize - 1;
                if idx < starts.len() {
                    by_line.entry(idx).or_default().push(LineRange {
                        start: s.column,
                        end: s.end_column,
                    });
                }
            }
            for (idx, mut ranges) in by_line.into_iter().take(max_lines as usize) {
                ranges.sort();
                ranges.dedup();
                let line = line_text(content, &starts, idx, self.limits.max_line_bytes);
                let cap = line.len() as u32;
                let ranges: Vec<LineRange> = ranges
                    .into_iter()
                    .filter(|r| r.start <= cap)
                    .map(|r| LineRange {
                        start: r.start,
                        end: r.end.min(cap),
                    })
                    .collect();
                let before = (idx.saturating_sub(context)..idx)
                    .map(|i| line_text(content, &starts, i, self.limits.max_line_bytes))
                    .collect();
                let after = (idx + 1..(idx + 1 + context).min(starts.len()))
                    .map(|i| line_text(content, &starts, i, self.limits.max_line_bytes))
                    .collect();
                line_matches.push(LineMatch {
                    line_number: idx as u32 + 1,
                    column: ranges.first().map_or(0, |r| r.start),
                    line,
                    ranges,
                    context_before: before,
                    context_after: after,
                });
            }
        }
        CodeHit {
            repo: t.repo,
            repo_full_name: t.full_name(),
            path: m.path.clone(),
            ref_name: t.ref_name.clone(),
            changeset: m.manifest.changeset,
            blob: m.entry.blob,
            language: m.entry.language,
            line_matches,
            match_count: (m.ranges.len() + m.symbols.len()) as u32,
            path_matched: m.path_matched,
            symbols: m.symbols.clone(),
            score: m.rank as f32,
        }
    }

    fn page_size(&self, requested: u32) -> usize {
        match requested {
            0 => self.limits.default_page_size,
            n => n.min(self.limits.max_page_size),
        }
        .max(1) as usize
    }

    /// Runs a code search and returns one page.
    pub fn search_code(
        &self,
        req: &CodeSearchRequest,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
    ) -> Result<CodeSearchResponse> {
        let q = Self::parse(&req.query)?;
        let compiled = compile(&q, req.case_sensitive)?;
        let after = if req.page_token.is_empty() {
            None
        } else {
            Some(Cursor::decode(&req.page_token, compiled.fingerprint)?.key())
        };
        let (matches, skipped) = self.run(&compiled, targets, auth)?;
        let total = matches.len() as u64;
        let facets = if req.include_facets {
            Self::facets(&matches.iter().collect::<Vec<_>>(), targets)
        } else {
            Vec::new()
        };
        let size = self.page_size(req.page_size);
        let start = match &after {
            None => 0,
            Some(k) => matches.partition_point(|m| Self::file_key(m, targets) <= *k),
        };
        let page: Vec<&FileMatch> = matches.iter().skip(start).take(size).collect();
        let max_lines = match req.max_line_matches_per_file {
            0 => self.limits.default_line_matches,
            n => n,
        };
        let hits: Vec<CodeHit> = page
            .iter()
            .map(|m| self.shape(m, targets, req.context_lines, max_lines))
            .collect();
        let next_page_token = if start + page.len() < matches.len() {
            page.last().map(|m| {
                let t = &targets[m.target];
                Cursor {
                    fingerprint: compiled.fingerprint,
                    rank: m.rank,
                    repo: t.full_name(),
                    ref_name: t.ref_name.clone(),
                    path: m.path.as_bytes().to_vec(),
                    line: 0,
                    column: 0,
                }
                .encode()
            })
        } else {
            None
        };
        Ok(CodeSearchResponse {
            hits,
            total_count: total,
            incomplete: !skipped.is_empty(),
            skipped_repos: skipped,
            facets,
            next_page_token,
        })
    }

    /// Streams every match file by file (in rank order) without paging, stopping once
    /// `max_matches` matches were sent (0 = no limit).
    pub fn stream_code(
        &self,
        req: &CodeSearchRequest,
        max_matches: u64,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
        on_hit: &mut dyn FnMut(CodeHit),
    ) -> Result<StreamSummary> {
        let q = Self::parse(&req.query)?;
        let compiled = compile(&q, req.case_sensitive)?;
        let (matches, skipped) = self.run(&compiled, targets, auth)?;
        let mut files = 0u64;
        let mut count = 0u64;
        let mut limit_hit = false;
        let mut sent = Vec::new();
        for (i, m) in matches.iter().enumerate() {
            if max_matches > 0 && count >= max_matches {
                limit_hit = true;
                break;
            }
            let hit = self.shape(m, targets, req.context_lines, u32::MAX);
            count += u64::from(hit.match_count);
            files += 1;
            sent.push(i);
            on_hit(hit);
        }
        let facets = if req.include_facets {
            let subset: Vec<&FileMatch> = sent.iter().map(|i| &matches[*i]).collect();
            Self::facets(&subset, targets)
        } else {
            Vec::new()
        };
        Ok(StreamSummary {
            files_matched: files,
            matches: count,
            limit_hit,
            skipped_repos: skipped,
            facets,
        })
    }

    fn location(
        m: &FileMatch,
        t: &RepoTarget,
        line: u32,
        column: u32,
        end: u32,
        symbol: Option<Symbol>,
        max: usize,
    ) -> SymbolLocation {
        let text = m
            .doc
            .as_ref()
            .map(|d| {
                let starts = line_starts(&d.content);
                let idx = (line as usize).saturating_sub(1).min(starts.len() - 1);
                line_text(&d.content, &starts, idx, max)
            })
            .unwrap_or_default();
        SymbolLocation {
            repo: t.repo,
            repo_full_name: t.full_name(),
            ref_name: t.ref_name.clone(),
            changeset: m.manifest.changeset,
            path: m.path.clone(),
            line_number: line,
            column,
            end_column: end,
            line: text,
            symbol,
            language: m.entry.language,
        }
    }

    /// Searches symbol definitions. Bare terms of `query` name symbols (literal = exact
    /// name, `/regex/` = regex over names); `repo:`, `org:`, `path:`, `lang:` and `case:`
    /// filter as in code search. `kinds` restricts symbol kinds (empty = all).
    pub fn search_symbols(
        &self,
        query: &str,
        kinds: &[SymbolKind],
        page_size: u32,
        page_token: &str,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
    ) -> Result<SymbolSearchResponse> {
        let mut q = Self::parse(query)?;
        q.expr = to_symbol_expr(q.expr);
        let compiled = compile(&q, false)?;
        if !compiled.has_symbol_terms {
            return Err(BeeError::new(
                Reason::InvalidArgument,
                "a symbol search needs at least one symbol name",
            )
            .with_payload("subcode", crate::query::BAD_QUERY));
        }
        let (matches, skipped) = self.run(&compiled, targets, auth)?;
        let mut locs: Vec<(SortKey, SymbolLocation)> = Vec::new();
        for m in &matches {
            let t = &targets[m.target];
            for s in &m.symbols {
                if !kinds.is_empty() && !kinds.contains(&s.kind) {
                    continue;
                }
                let key = (
                    Reverse(0),
                    t.full_name(),
                    t.ref_name.clone(),
                    m.path.as_bytes().to_vec(),
                    s.line,
                    s.column,
                );
                let loc = Self::location(
                    m,
                    t,
                    s.line,
                    s.column,
                    s.end_column,
                    Some(s.clone()),
                    self.limits.max_line_bytes,
                );
                locs.push((key, loc));
            }
        }
        locs.sort_by(|a, b| a.0.cmp(&b.0));
        let total = locs.len() as u64;
        let start = if page_token.is_empty() {
            0
        } else {
            let k = Cursor::decode(page_token, compiled.fingerprint)?.key();
            locs.partition_point(|(key, _)| *key <= k)
        };
        let size = self.page_size(page_size);
        let page: Vec<(SortKey, SymbolLocation)> =
            locs.into_iter().skip(start).take(size + 1).collect();
        let more = page.len() > size;
        let page: Vec<(SortKey, SymbolLocation)> = page.into_iter().take(size).collect();
        let next_page_token = if more {
            page.last().map(|(k, _)| {
                Cursor {
                    fingerprint: compiled.fingerprint,
                    rank: 0,
                    repo: k.1.clone(),
                    ref_name: k.2.clone(),
                    path: k.3.clone(),
                    line: k.4,
                    column: k.5,
                }
                .encode()
            })
        } else {
            None
        };
        Ok(SymbolSearchResponse {
            symbols: page.into_iter().map(|(_, l)| l).collect(),
            total_count: total,
            incomplete: !skipped.is_empty(),
            next_page_token,
        })
    }

    /// Definitions of a symbol name (exact, case-sensitive) across the targets.
    pub fn definitions(
        &self,
        name: &str,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
    ) -> Result<Vec<SymbolLocation>> {
        let q = CodeQuery {
            expr: Expr::Term(CodeTerm::Symbol(Pattern::Literal(name.to_string()))),
            case_sensitive: true,
            qualifiers: Vec::new(),
        };
        let compiled = compile(&q, true)?;
        let (matches, _) = self.run(&compiled, targets, auth)?;
        let mut out = Vec::new();
        for m in &matches {
            let t = &targets[m.target];
            for s in &m.symbols {
                out.push(Self::location(
                    m,
                    t,
                    s.line,
                    s.column,
                    s.end_column,
                    Some(s.clone()),
                    self.limits.max_line_bytes,
                ));
            }
        }
        out.sort_by(|a, b| {
            (
                &a.repo_full_name,
                &a.ref_name,
                a.path.as_bytes(),
                a.line_number,
                a.column,
            )
                .cmp(&(
                    &b.repo_full_name,
                    &b.ref_name,
                    b.path.as_bytes(),
                    b.line_number,
                    b.column,
                ))
        });
        Ok(out)
    }

    /// Search-based references: every whole-word, case-sensitive occurrence of `name` in
    /// files the caller can read, ordered by repo, ref, path and position. Definitions are
    /// included only with `include_declaration`. `filter` is an optional code query
    /// (e.g. `lang:rust path:src/`) that further restricts the files.
    pub fn references(
        &self,
        name: &str,
        include_declaration: bool,
        filter: &str,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
    ) -> Result<Vec<SymbolLocation>> {
        let word = Expr::Term(CodeTerm::Content(Pattern::Regex(format!(
            r"\b{}\b",
            regex::escape(name)
        ))));
        let expr = if filter.trim().is_empty() {
            word
        } else {
            let f = Self::parse(filter)?;
            Expr::And(vec![word, f.expr])
        };
        let q = CodeQuery {
            expr,
            case_sensitive: true,
            qualifiers: Vec::new(),
        };
        let compiled = compile(&q, true)?;
        let (matches, _) = self.run(&compiled, targets, auth)?;
        let mut out = Vec::new();
        for m in &matches {
            let t = &targets[m.target];
            let Some(doc) = &m.doc else { continue };
            let starts = line_starts(&doc.content);
            let defs: Vec<Symbol> = match m.entry.language {
                Some(l) => symbols::extract(&doc.content, l)
                    .into_iter()
                    .filter(|s| s.name == name)
                    .collect(),
                None => Vec::new(),
            };
            for (s, e, _) in &m.ranges {
                let idx = starts.partition_point(|x| *x <= *s) - 1;
                let line = idx as u32 + 1;
                let col = (s - starts[idx]) as u32;
                let end = (e - starts[idx]) as u32;
                let def = defs
                    .iter()
                    .find(|d| d.line == line && d.column == col)
                    .cloned();
                if def.is_some() && !include_declaration {
                    continue;
                }
                out.push(Self::location(
                    m,
                    t,
                    line,
                    col,
                    end,
                    def,
                    self.limits.max_line_bytes,
                ));
            }
        }
        out.sort_by(|a, b| {
            (
                &a.repo_full_name,
                &a.ref_name,
                a.path.as_bytes(),
                a.line_number,
                a.column,
            )
                .cmp(&(
                    &b.repo_full_name,
                    &b.ref_name,
                    b.path.as_bytes(),
                    b.line_number,
                    b.column,
                ))
        });
        Ok(out)
    }

    /// The symbol outline of one file (`NOT_FOUND` when the file is absent, not indexed or
    /// not readable by the caller, so existence cannot be probed).
    pub fn document_symbols(
        &self,
        target: &RepoTarget,
        path: &RepoPath,
        auth: &dyn PathAuthorizer,
    ) -> Result<(Option<Language>, ObjectId, Vec<Symbol>)> {
        let not_found = || BeeError::new(Reason::NotFound, format!("{path} is not indexed"));
        if !auth.can_read(target.repo, path) {
            return Err(not_found());
        }
        let m = self
            .store
            .manifest(target.org, target.repo, &target.ref_name)?
            .ok_or_else(not_found)?;
        let entry = m.files.get(path).ok_or_else(not_found)?;
        let syms = match (entry.doc, entry.language) {
            (Some(n), Some(l)) => match self.store.document(target.org, n)? {
                Some(d) if d.skip.is_none() => symbols::extract(&d.content, l),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        Ok((entry.language, m.changeset, syms))
    }

    /// Type-ahead suggestions for a prefix: repos whose name starts with it, readable
    /// paths whose file name starts with it, and (for prefixes of three or more bytes)
    /// symbols in readable files whose name starts with it. Case-insensitive, ordered by
    /// kind then text, at most `limit`.
    pub fn suggest(
        &self,
        prefix: &str,
        limit: usize,
        targets: &[RepoTarget],
        auth: &dyn PathAuthorizer,
    ) -> Result<Vec<Suggestion>> {
        let p = prefix.to_lowercase();
        if p.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = std::collections::BTreeSet::new();
        for t in targets {
            if t.repo_name.to_lowercase().starts_with(&p)
                || t.full_name().to_lowercase().starts_with(&p)
            {
                out.insert(Suggestion {
                    kind: SuggestionKind::Repo,
                    text: t.full_name(),
                    repo_full_name: String::new(),
                });
            }
        }
        let symbol_plan = if p.len() >= 3 {
            Some(TrigramQuery::of_literal(p.as_bytes()))
        } else {
            None
        };
        let mut cands: BTreeMap<OrgId, Option<Vec<DocNum>>> = BTreeMap::new();
        for t in targets {
            let Some(m) = self.store.manifest(t.org, t.repo, &t.ref_name)? else {
                continue;
            };
            if let Some(plan) = &symbol_plan
                && let std::collections::btree_map::Entry::Vacant(e) = cands.entry(t.org)
            {
                e.insert(candidates(self.store.as_ref(), t.org, plan)?);
            }
            for (path, entry) in &m.files {
                let name = String::from_utf8_lossy(path.file_name()).to_lowercase();
                let path_hit = name.starts_with(&p);
                let symbol_candidate = symbol_plan.is_some()
                    && entry.language.is_some_and(symbols::supports)
                    && match (cands.get(&t.org).and_then(Option::as_ref), entry.doc) {
                        (None, Some(_)) => true,
                        (Some(c), Some(d)) => c.binary_search(&d).is_ok(),
                        _ => false,
                    };
                if !path_hit && !symbol_candidate {
                    continue;
                }
                if !auth.can_read(t.repo, path) {
                    continue;
                }
                if path_hit {
                    out.insert(Suggestion {
                        kind: SuggestionKind::Path,
                        text: path.to_string(),
                        repo_full_name: t.full_name(),
                    });
                }
                if symbol_candidate
                    && let (Some(n), Some(l)) = (entry.doc, entry.language)
                    && let Some(d) = self.store.document(t.org, n)?
                {
                    for s in symbols::extract(&d.content, l) {
                        if s.name.to_lowercase().starts_with(&p) {
                            out.insert(Suggestion {
                                kind: SuggestionKind::Symbol,
                                text: s.name,
                                repo_full_name: t.full_name(),
                            });
                        }
                    }
                }
            }
        }
        Ok(out.into_iter().take(limit).collect())
    }
}

/// Turns free-text terms into symbol terms (for symbol search).
fn to_symbol_expr(e: Expr<CodeTerm>) -> Expr<CodeTerm> {
    match e {
        Expr::Term(CodeTerm::Content(p)) => Expr::Term(CodeTerm::Symbol(p)),
        Expr::Term(t) => Expr::Term(t),
        Expr::Not(inner) => Expr::Not(Box::new(to_symbol_expr(*inner))),
        Expr::And(v) => Expr::And(v.into_iter().map(to_symbol_expr).collect()),
        Expr::Or(v) => Expr::Or(v.into_iter().map(to_symbol_expr).collect()),
    }
}
