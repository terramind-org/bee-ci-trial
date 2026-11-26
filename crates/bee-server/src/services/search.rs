//! `SearchService` (`docs/spec/search.md`, `docs/spec/operations.md` §2).
//!
//! **Code search** runs on a trigram index ([`bee_search::DiskIndexStore`] under
//! `search.index_dir`, default `<data>/search`). The indexer task watches every repo's event
//! log head (lands and ref moves append events) and brings the manifests of the configured
//! refs (`search.refs`, default `main`) up to date incrementally from changeset deltas. Every
//! query passes only the repos the caller may read, and every file passes the caller's path
//! permission (SPEC §9.3) before it is read, counted or ranked.
//!
//! The index is a **separate failure domain**: when it cannot be opened or read, code, symbol
//! and navigation queries answer `UNAVAILABLE` (retryable) and the indexer keeps retrying;
//! nothing else depends on it. Issue, change request, repo, changeset, user, topic,
//! discussion and package search read the stores the list RPCs read (the forge's own lists,
//! called in-process with the caller's credentials, and the VCS history).
//!
//! Saved searches are per principal rows of the server store. Trending ranks the caller's
//! readable repos by lands in the window, then stars; leaderboards rank agents, humans or
//! repos by lands, reviews or stars gained in the window; topics aggregate repo topics;
//! featured topics and collections come from `[search]` configuration.
//!
//! A caller without credentials (`forge.allow_anonymous_public_read`, `crate::access`) reads
//! every search, trending, leaderboard and topic RPC over public repos only: its readable set
//! is decided with the empty principal chain. Saved searches need an account.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use bee_core::{ObjectId, RefName, RepoPath};
use bee_history::{LogOptions, PathContent};
use bee_policy::{Action, Policy};
use bee_proto::v1::search_service_server::SearchService;
use bee_proto::v1::{self, ErrorReason};
use bee_search::forge::{CompiledFilter, Filter, FilterContext, Predicate};
use bee_search::search::PathAuthorizer;
use bee_search::{
    CodeSearchRequest, DiskIndexStore, ForgeKind, IndexStore, Indexer, QueryKind, RepoTarget,
    Searcher,
};
use bee_store::RepoId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tonic::metadata::MetadataMap;

use crate::auth::Caller;
use crate::convert::id_to;
use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::services::object::finish;
use crate::services::support::{all_repos, get_json, next_page, offset_page, page_of, scan_json};
use crate::services::{Rpc, RpcStream, stream_of};
use crate::state::{AppState, RepoCtx, RequestMeta};
use crate::util::timestamp_ms;

/// Saved searches (`<principal>/<id>`).
pub const NS_SAVED: &str = "saved_search";
/// Saved search names (`<principal>/<name>` → id).
pub const NS_SAVED_NAME: &str = "saved_search_name";

/// Most repos one forge search examines.
const MAX_REPOS: usize = 1000;
/// Most items read per repo by a forge search.
const MAX_PER_REPO: usize = 2000;

// ----- the index ---------------------------------------------------------------------------

/// The code index of this server.
pub struct SearchIndex {
    dir: std::path::PathBuf,
    store: RwLock<Option<Arc<DiskIndexStore>>>,
    down: AtomicBool,
    last_error: Mutex<String>,
    heads: Mutex<HashMap<RepoId, u64>>,
}

fn unavailable(msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorReason::Unavailable, msg)
}

impl SearchIndex {
    /// The index of `state` (opened on first use).
    pub fn new(state: &AppState) -> SearchIndex {
        let dir = if state.cfg.search.index_dir.is_empty() {
            state.cfg.server.data_dir.join("search")
        } else {
            std::path::PathBuf::from(&state.cfg.search.index_dir)
        };
        SearchIndex {
            dir,
            store: RwLock::new(None),
            down: AtomicBool::new(false),
            last_error: Mutex::new(String::new()),
            heads: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the last open or read failed.
    pub fn is_down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
    }

    /// The last failure.
    pub fn last_error(&self) -> String {
        self.last_error
            .lock()
            .map(|e| e.clone())
            .unwrap_or_default()
    }

    fn fail(&self, e: &str) -> ApiError {
        self.down.store(true, Ordering::Relaxed);
        if let Ok(mut l) = self.last_error.lock() {
            *l = e.to_string();
        }
        // Drop the handle so the next use reopens from disk.
        if let Ok(mut s) = self.store.write() {
            *s = None;
        }
        unavailable(format!("code search is unavailable: {e}"))
    }

    fn ok(&self) {
        self.down.store(false, Ordering::Relaxed);
    }

    /// The open store, opening it if needed.
    pub fn store(&self) -> ApiResult<Arc<DiskIndexStore>> {
        if let Some(s) = self.store.read().ok().and_then(|g| g.clone()) {
            return Ok(s);
        }
        match DiskIndexStore::open(self.dir.clone()) {
            Ok(s) => {
                let s = Arc::new(s);
                if let Ok(mut g) = self.store.write() {
                    *g = Some(s.clone());
                }
                Ok(s)
            }
            Err(e) => Err(self.fail(&e.message)),
        }
    }

    /// Runs a blocking index operation, mapping store failures to `UNAVAILABLE` and marking
    /// the index down; query errors (`INVALID_ARGUMENT`, `TOO_LARGE`, `NOT_FOUND`) pass.
    async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Searcher) -> bee_core::Result<T> + Send + 'static,
    ) -> ApiResult<T> {
        let store = self.store()?;
        let me = self.clone();
        let out = tokio::task::spawn_blocking(move || {
            let searcher = Searcher::new(store as Arc<dyn IndexStore>);
            f(&searcher)
        })
        .await
        .map_err(|e| unavailable(format!("search task: {e}")))?;
        match out {
            Ok(v) => {
                me.ok();
                Ok(v)
            }
            Err(e)
                if matches!(
                    e.reason,
                    bee_core::Reason::Unavailable | bee_core::Reason::Integrity
                ) =>
            {
                Err(me.fail(&e.message))
            }
            Err(e) => Err(ApiError::from(e)),
        }
    }

    /// One indexing pass: every repo whose event head moved gets its refs synced.
    pub async fn index_pass(&self, state: &AppState) -> ApiResult<u64> {
        let store = self.store()?;
        let indexer = Indexer::new(
            store as Arc<dyn IndexStore>,
            state.meta.clone(),
            state.blobs.clone(),
        );
        let mut synced = 0u64;
        for repo in all_repos(state).await? {
            let head = state.meta.event_head(repo.repo.id).await?.written;
            let seen = self
                .heads
                .lock()
                .ok()
                .and_then(|h| h.get(&repo.repo.id).copied());
            if seen == Some(head) {
                continue;
            }
            let mut ok = true;
            for r in &state.cfg.search.refs {
                let Ok(name) = RefName::new(r.as_str()) else {
                    continue;
                };
                if let Err(e) = indexer.sync_ref(repo.org.id, repo.repo.id, &name).await {
                    ok = false;
                    if matches!(
                        e.reason,
                        bee_core::Reason::Unavailable | bee_core::Reason::Integrity
                    ) {
                        return Err(self.fail(&e.message));
                    }
                    logging::log(
                        Level::Warn,
                        "search",
                        "indexing a ref failed",
                        &[("repo", repo.id_text().into()), ("error", e.message.into())],
                    );
                }
            }
            if ok {
                synced += 1;
                if let Ok(mut h) = self.heads.lock() {
                    h.insert(repo.repo.id, head);
                }
            }
        }
        self.ok();
        Ok(synced)
    }
}

/// The indexer task: polls every `search.poll_ms` until the server goes away.
fn spawn_indexer(state: Weak<AppState>, index: Weak<SearchIndex>) {
    tokio::spawn(async move {
        loop {
            let every = match state.upgrade() {
                Some(s) => Duration::from_millis(s.cfg.search.poll_ms.max(20)),
                None => return,
            };
            tokio::time::sleep(every).await;
            let (Some(s), Some(ix)) = (state.upgrade(), index.upgrade()) else {
                return;
            };
            if let Err(e) = ix.index_pass(&s).await {
                logging::log(
                    Level::Warn,
                    "search",
                    "indexing pass failed",
                    &[("error", e.detail.message.into())],
                );
            }
        }
    });
}

// ----- the service --------------------------------------------------------------------------

/// The search service.
pub struct SearchApi {
    state: Arc<AppState>,
    index: Arc<SearchIndex>,
}

impl SearchApi {
    /// The service over `state`; starts the indexer when `search.enabled`.
    pub fn new(state: Arc<AppState>) -> SearchApi {
        let index = Arc::new(SearchIndex::new(&state));
        if state.cfg.search.enabled {
            spawn_indexer(Arc::downgrade(&state), Arc::downgrade(&index));
        }
        crate::services::search::register_index(&state, &index);
        SearchApi { state, index }
    }

    fn code_enabled(&self) -> ApiResult<()> {
        if self.state.cfg.search.enabled {
            Ok(())
        } else {
            Err(unavailable(
                "code search is disabled on this server (search.enabled)",
            ))
        }
    }
}

static INDEXES: Mutex<Vec<(Weak<AppState>, Weak<SearchIndex>)>> = Mutex::new(Vec::new());

fn register_index(state: &Arc<AppState>, index: &Arc<SearchIndex>) {
    if let Ok(mut v) = INDEXES.lock() {
        v.retain(|(s, i)| s.strong_count() > 0 && i.strong_count() > 0);
        v.push((Arc::downgrade(state), Arc::downgrade(index)));
    }
}

/// The code index of a server, for health reports.
pub fn index_of(state: &Arc<AppState>) -> Option<Arc<SearchIndex>> {
    let v = INDEXES.lock().ok()?;
    v.iter()
        .find(|(s, _)| s.upgrade().is_some_and(|s| Arc::ptr_eq(&s, state)))
        .and_then(|(_, i)| i.upgrade())
}

/// What a code query may see.
struct Scope {
    targets: Vec<RepoTarget>,
    policies: HashMap<RepoId, (Arc<Policy>, RepoCtx)>,
}

struct Auth {
    state: Arc<AppState>,
    caller: Caller,
    policies: HashMap<RepoId, Arc<Policy>>,
}

impl PathAuthorizer for Auth {
    fn can_read(&self, repo: RepoId, path: &RepoPath) -> bool {
        self.policies.get(&repo).is_some_and(|p| {
            self.state
                .path_allowed(&self.caller, repo, p, Action::Read, &path.to_string())
        })
    }
}

impl SearchApi {
    /// The repos the caller may read, with their policies (optionally only `only`).
    async fn scope(&self, caller: &Caller, only: Option<&RepoCtx>) -> ApiResult<Scope> {
        let s = &self.state;
        let repos = match only {
            Some(r) => vec![r.clone()],
            None => all_repos(s).await?,
        };
        let mut targets = Vec::new();
        let mut policies = HashMap::new();
        for r in repos.into_iter().take(MAX_REPOS) {
            let Ok(policy) = s.gate(caller, &r, Action::Read, None).await else {
                continue;
            };
            for name in &s.cfg.search.refs {
                targets.push(RepoTarget {
                    org: r.org.id,
                    org_name: r.org.name.clone(),
                    repo: r.repo.id,
                    repo_name: r.repo.name.clone(),
                    ref_name: name.clone(),
                });
            }
            policies.insert(r.repo.id, (policy, r));
        }
        Ok(Scope { targets, policies })
    }

    fn auth(&self, caller: &Caller, scope: &Scope) -> Auth {
        Auth {
            state: self.state.clone(),
            caller: caller.clone(),
            policies: scope
                .policies
                .iter()
                .map(|(k, (p, _))| (*k, p.clone()))
                .collect(),
        }
    }
}

fn repo_ref(full_name: &str) -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName(full_name.to_string())),
    })
}

fn symbol_kind_to(k: bee_search::symbols::SymbolKind) -> i32 {
    let name = format!("SYMBOL_KIND_{}", k.name().to_ascii_uppercase());
    v1::SymbolKind::from_str_name(&name).unwrap_or(v1::SymbolKind::Unspecified) as i32
}

fn symbol_kind_from(k: i32) -> Option<bee_search::symbols::SymbolKind> {
    let name = v1::SymbolKind::try_from(k).ok()?.as_str_name();
    bee_search::symbols::SymbolKind::parse(name.trim_start_matches("SYMBOL_KIND_"))
}

fn symbol_info(
    s: &bee_search::symbols::Symbol,
    lang: Option<bee_search::lang::Language>,
) -> v1::SymbolInfo {
    v1::SymbolInfo {
        name: s.name.clone(),
        qualified_name: lang
            .map(|l| s.qualified_name(l))
            .unwrap_or_else(|| s.name.clone()),
        kind: symbol_kind_to(s.kind),
        language: lang.map(|l| l.name().to_string()).unwrap_or_default(),
        container: s.container.clone().unwrap_or_default(),
        signature: s.signature.clone(),
    }
}

fn hit_to(h: &bee_search::search::CodeHit) -> v1::CodeSearchHit {
    v1::CodeSearchHit {
        repo: Some(v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(h.repo.0.to_text())),
        }),
        repo_full_name: h.repo_full_name.clone(),
        path: h.path.to_string().into_bytes(),
        r#ref: h.ref_name.clone(),
        changeset: Some(id_to(&h.changeset)),
        blob: Some(id_to(&h.blob)),
        language: h.language.map(|l| l.name().to_string()).unwrap_or_default(),
        line_matches: h
            .line_matches
            .iter()
            .map(|m| v1::CodeSearchLineMatch {
                line_number: m.line_number,
                line: m.line.clone(),
                ranges: m
                    .ranges
                    .iter()
                    .map(|r| v1::SearchRange {
                        start: r.start,
                        end: r.end,
                    })
                    .collect(),
                context_before: m.context_before.clone(),
                context_after: m.context_after.clone(),
            })
            .collect(),
        match_count: h.match_count,
        path_matched: h.path_matched,
        symbols: h
            .symbols
            .iter()
            .map(|s| symbol_info(s, h.language))
            .collect(),
        score: h.score,
    }
}

fn facets_to(f: &[bee_search::search::Facet]) -> Vec<v1::SearchFacet> {
    f.iter()
        .map(|f| v1::SearchFacet {
            name: f.name.clone(),
            values: f
                .values
                .iter()
                .map(|(v, c)| v1::SearchFacetValue {
                    value: v.clone(),
                    count: *c,
                })
                .collect(),
        })
        .collect()
}

fn location_to(l: &bee_search::search::SymbolLocation) -> v1::SymbolLocation {
    v1::SymbolLocation {
        repo: Some(v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(l.repo.0.to_text())),
        }),
        repo_full_name: l.repo_full_name.clone(),
        path: l.path.to_string().into_bytes(),
        changeset: Some(id_to(&l.changeset)),
        range: Some(v1::SymbolRange {
            start_line: l.line_number,
            start_column: l.column,
            end_line: l.line_number,
            end_column: l.end_column,
        }),
        line: l.line.clone(),
        symbol: l.symbol.as_ref().map(|s| symbol_info(s, l.language)),
        precise: false,
    }
}

/// The word (identifier) at a 1-based line and 0-based byte column.
fn word_at(text: &str, line: u32, column: u32) -> Option<String> {
    let l = text.lines().nth(line.checked_sub(1)? as usize)?;
    let b = l.as_bytes();
    let c = column as usize;
    let is_word = |x: u8| x.is_ascii_alphanumeric() || x == b'_';
    // The column names a byte; it must be part of an identifier.
    if !b.get(c).is_some_and(|&x| is_word(x)) {
        return None;
    }
    let mut start = c;
    while start > 0 && is_word(b[start - 1]) {
        start -= 1;
    }
    let mut end = c;
    while end < b.len() && is_word(b[end]) {
        end += 1;
    }
    (end > start).then(|| l[start..end].to_string())
}

impl SearchApi {
    /// A file's text at a revision, readable by the caller (`NOT_FOUND` otherwise).
    async fn file_at(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        at: Option<&v1::Revision>,
        path: &[u8],
    ) -> ApiResult<(String, ObjectId, String)> {
        let s = &self.state;
        let policy = s.gate(caller, repo, Action::Read, None).await?;
        let path_text = String::from_utf8(path.to_vec())
            .map_err(|_| ApiError::invalid("invalid_path", "paths are UTF-8"))?;
        let rp = RepoPath::new(path_text.trim_start_matches('/')).map_err(ApiError::from)?;
        if !s.path_allowed(caller, repo.repo.id, &policy, Action::Read, &rp.to_string()) {
            return Err(ApiError::not_found(format!("{rp} not found")));
        }
        let history = s.history(repo);
        let cs = crate::services::query::resolve_rev(&history, at).await?;
        match history.read_path(&cs, Some(&rp)).await? {
            PathContent::File { content, .. } => Ok((
                String::from_utf8_lossy(content.bytes_or_empty()).into_owned(),
                cs,
                rp.to_string(),
            )),
            _ => Err(ApiError::not_found(format!("{rp} is not a file"))),
        }
    }

    async fn search_code_impl(
        &self,
        caller: &Caller,
        r: v1::SearchCodeRequest,
    ) -> ApiResult<v1::SearchCodeResponse> {
        self.code_enabled()?;
        let scope = self.scope(caller, None).await?;
        let auth = self.auth(caller, &scope);
        let (token, size) = page_of(&r.page);
        let req = CodeSearchRequest {
            query: r.query,
            case_sensitive: r.case_sensitive,
            context_lines: r.context_lines,
            max_line_matches_per_file: r.max_line_matches_per_file,
            include_facets: r.include_facets,
            page_size: size as u32,
            page_token: token,
        };
        let targets = scope.targets;
        let resp = self
            .index
            .run(move |s| s.search_code(&req, &targets, &auth))
            .await?;
        Ok(v1::SearchCodeResponse {
            hits: resp.hits.iter().map(hit_to).collect(),
            total_count: resp.total_count,
            incomplete: resp.incomplete,
            facets: facets_to(&resp.facets),
            page: next_page(resp.next_page_token.unwrap_or_default()),
        })
    }

    async fn stream_code_impl(
        &self,
        caller: &Caller,
        r: v1::StreamSearchCodeRequest,
    ) -> ApiResult<Vec<v1::CodeSearchStreamMessage>> {
        self.code_enabled()?;
        let scope = self.scope(caller, None).await?;
        let auth = self.auth(caller, &scope);
        let repos_total = scope.policies.len() as u32;
        let req = CodeSearchRequest {
            query: r.query,
            case_sensitive: r.case_sensitive,
            context_lines: r.context_lines,
            include_facets: r.include_facets,
            ..CodeSearchRequest::default()
        };
        let max = if r.max_matches == 0 {
            10_000
        } else {
            r.max_matches
        };
        let targets = scope.targets;
        let (hits, summary) = self
            .index
            .run(move |s| {
                let mut hits = Vec::new();
                let summary = s.stream_code(&req, max, &targets, &auth, &mut |h| hits.push(h))?;
                Ok((hits, summary))
            })
            .await?;
        let mut out: Vec<v1::CodeSearchStreamMessage> = Vec::new();
        use v1::code_search_stream_message::Payload;
        let mut files = 0u64;
        let mut matches = 0u64;
        for h in &hits {
            files += 1;
            matches += u64::from(h.match_count);
            out.push(v1::CodeSearchStreamMessage {
                payload: Some(Payload::Hit(hit_to(h))),
            });
            if files.is_multiple_of(100) {
                out.push(v1::CodeSearchStreamMessage {
                    payload: Some(Payload::Progress(v1::CodeSearchStreamProgress {
                        files_matched: files,
                        matches,
                        repos_searched: repos_total,
                        repos_total,
                    })),
                });
            }
        }
        out.push(v1::CodeSearchStreamMessage {
            payload: Some(Payload::Done(v1::CodeSearchStreamDone {
                files_matched: summary.files_matched,
                matches: summary.matches,
                limit_hit: summary.limit_hit,
                skipped_repos: summary.skipped_repos,
                facets: facets_to(&summary.facets),
            })),
        });
        Ok(out)
    }

    async fn search_symbols_impl(
        &self,
        caller: &Caller,
        r: v1::SearchSymbolsRequest,
    ) -> ApiResult<v1::SearchSymbolsResponse> {
        self.code_enabled()?;
        let scope = self.scope(caller, None).await?;
        let auth = self.auth(caller, &scope);
        let (token, size) = page_of(&r.page);
        let kinds: Vec<_> = r
            .kinds
            .iter()
            .filter_map(|k| symbol_kind_from(*k))
            .collect();
        let targets = scope.targets;
        let q = r.query;
        let resp = self
            .index
            .run(move |s| s.search_symbols(&q, &kinds, size as u32, &token, &targets, &auth))
            .await?;
        Ok(v1::SearchSymbolsResponse {
            symbols: resp.symbols.iter().map(location_to).collect(),
            total_count: resp.total_count,
            incomplete: resp.incomplete,
            page: next_page(resp.next_page_token.unwrap_or_default()),
        })
    }

    async fn definitions_impl(
        &self,
        caller: &Caller,
        r: v1::GetSymbolDefinitionsRequest,
    ) -> ApiResult<v1::GetSymbolDefinitionsResponse> {
        self.code_enabled()?;
        let repo = self.state.repo(r.repo.as_ref()).await?;
        let (text, _, path) = self.file_at(caller, &repo, r.at.as_ref(), &r.path).await?;
        let Some(name) = word_at(&text, r.line, r.column) else {
            return Ok(v1::GetSymbolDefinitionsResponse::default());
        };
        let lang = bee_search::lang::detect(path.as_bytes(), text.as_bytes());
        let local = lang
            .map(|l| bee_search::symbols::extract(text.as_bytes(), l))
            .unwrap_or_default();
        let symbol = local
            .iter()
            .find(|s| s.name == name)
            .map(|s| symbol_info(s, lang));
        let scope = self.scope(caller, (!r.cross_repo).then_some(&repo)).await?;
        let auth = self.auth(caller, &scope);
        let targets = scope.targets;
        let n = name.clone();
        let defs = self
            .index
            .run(move |s| s.definitions(&n, &targets, &auth))
            .await?;
        Ok(v1::GetSymbolDefinitionsResponse {
            definitions: defs.iter().map(location_to).collect(),
            symbol: symbol.or_else(|| {
                defs.first()
                    .and_then(|d| d.symbol.as_ref().map(|s| symbol_info(s, d.language)))
            }),
        })
    }

    async fn references_impl(
        &self,
        caller: &Caller,
        r: v1::GetSymbolReferencesRequest,
    ) -> ApiResult<v1::GetSymbolReferencesResponse> {
        self.code_enabled()?;
        let repo = self.state.repo(r.repo.as_ref()).await?;
        let (text, _, path) = self.file_at(caller, &repo, r.at.as_ref(), &r.path).await?;
        let Some(name) = word_at(&text, r.line, r.column) else {
            return Ok(v1::GetSymbolReferencesResponse::default());
        };
        let lang = bee_search::lang::detect(path.as_bytes(), text.as_bytes());
        let symbol = lang
            .map(|l| bee_search::symbols::extract(text.as_bytes(), l))
            .unwrap_or_default()
            .iter()
            .find(|s| s.name == name)
            .map(|s| symbol_info(s, lang));
        let scope = self.scope(caller, (!r.cross_repo).then_some(&repo)).await?;
        let auth = self.auth(caller, &scope);
        let targets = scope.targets;
        let decl = r.include_declaration;
        let all = self
            .index
            .run(move |s| s.references(&name, decl, "", &targets, &auth))
            .await?;
        let (token, size) = page_of(&r.page);
        let locs: Vec<v1::SymbolLocation> = all.iter().map(location_to).collect();
        let (page, next) = offset_page(&locs, &token, size)?;
        Ok(v1::GetSymbolReferencesResponse {
            references: page,
            symbol,
            page: next_page(next),
        })
    }

    async fn document_symbols_impl(
        &self,
        caller: &Caller,
        r: v1::GetDocumentSymbolsRequest,
    ) -> ApiResult<v1::GetDocumentSymbolsResponse> {
        let repo = self.state.repo(r.repo.as_ref()).await?;
        let (text, cs, path) = self.file_at(caller, &repo, r.at.as_ref(), &r.path).await?;
        let lang = bee_search::lang::detect(path.as_bytes(), text.as_bytes());
        let symbols = lang
            .map(|l| bee_search::symbols::extract(text.as_bytes(), l))
            .unwrap_or_default();
        Ok(v1::GetDocumentSymbolsResponse {
            symbols: outline(&symbols, lang, &text),
            language: lang.map(|l| l.name().to_string()).unwrap_or_default(),
            changeset: Some(id_to(&cs)),
        })
    }
}

/// Nests symbols under their containers (by name, nearest preceding container).
fn outline(
    symbols: &[bee_search::symbols::Symbol],
    lang: Option<bee_search::lang::Language>,
    text: &str,
) -> Vec<v1::SymbolOutlineNode> {
    let line_len = |n: u32| -> u32 {
        text.lines()
            .nth(n.saturating_sub(1) as usize)
            .map(|l| l.len() as u32)
            .unwrap_or(0)
    };
    let node = |s: &bee_search::symbols::Symbol| v1::SymbolOutlineNode {
        symbol: Some(symbol_info(s, lang)),
        range: Some(v1::SymbolRange {
            start_line: s.line,
            start_column: 0,
            end_line: s.line,
            end_column: line_len(s.line),
        }),
        selection_range: Some(v1::SymbolRange {
            start_line: s.line,
            start_column: s.column,
            end_line: s.line,
            end_column: s.end_column,
        }),
        children: Vec::new(),
    };
    let mut roots: Vec<v1::SymbolOutlineNode> = Vec::new();
    for s in symbols {
        let n = node(s);
        match &s.container {
            Some(c) => match roots
                .iter_mut()
                .rev()
                .find(|r| r.symbol.as_ref().is_some_and(|x| &x.name == c))
            {
                Some(parent) => parent.children.push(n),
                None => roots.push(n),
            },
            None => roots.push(n),
        }
    }
    roots
}

// ----- forge-backed searches ---------------------------------------------------------------

/// Evaluates a compiled forge filter with a predicate oracle.
fn eval(f: &Filter, m: &dyn Fn(&Predicate) -> bool) -> bool {
    match f {
        Filter::All => true,
        Filter::Pred(p) => m(p),
        Filter::Not(i) => !eval(i, m),
        Filter::And(v) => v.iter().all(|x| eval(x, m)),
        Filter::Or(v) => v.iter().any(|x| eval(x, m)),
    }
}

fn in_range(v: i64, min: &Bound<i64>, max: &Bound<i64>) -> bool {
    let lo = match min {
        Bound::Included(x) => v >= *x,
        Bound::Excluded(x) => v > *x,
        Bound::Unbounded => true,
    };
    let hi = match max {
        Bound::Included(x) => v <= *x,
        Bound::Excluded(x) => v < *x,
        Bound::Unbounded => true,
    };
    lo && hi
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

fn ts_ms(t: &Option<pbjson_types::Timestamp>) -> i64 {
    t.as_ref()
        .map(|t| t.seconds * 1000 + i64::from(t.nanos) / 1_000_000)
        .unwrap_or(0)
}

fn query_kind(k: i32) -> ApiResult<QueryKind> {
    Ok(
        match v1::SearchQueryKind::try_from(k).unwrap_or(v1::SearchQueryKind::Unspecified) {
            v1::SearchQueryKind::Code => QueryKind::Code,
            v1::SearchQueryKind::Symbols => QueryKind::Symbols,
            v1::SearchQueryKind::Issues => QueryKind::Forge(ForgeKind::Issues),
            v1::SearchQueryKind::Repos => QueryKind::Forge(ForgeKind::Repos),
            v1::SearchQueryKind::Changesets => QueryKind::Forge(ForgeKind::Changesets),
            v1::SearchQueryKind::Users => QueryKind::Forge(ForgeKind::Users),
            v1::SearchQueryKind::Topics => QueryKind::Forge(ForgeKind::Topics),
            v1::SearchQueryKind::Discussions => QueryKind::Forge(ForgeKind::Discussions),
            v1::SearchQueryKind::Packages => QueryKind::Forge(ForgeKind::Packages),
            v1::SearchQueryKind::Unspecified => {
                return Err(ApiError::invalid("missing_field", "kind is required"));
            }
        },
    )
}

/// The caller's login (for `@me`): its display name.
fn me_login(caller: &Caller) -> String {
    caller
        .record
        .as_ref()
        .map(|r| r.display.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| caller.id().to_string())
}

/// Parses and compiles a forge query.
fn compile(
    kind: ForgeKind,
    query: &str,
    caller: &Caller,
    now_ms: u64,
) -> ApiResult<CompiledFilter> {
    let q = bee_search::parse_forge_query(kind, query)
        .map_err(|e| ApiError::from(bee_search::query::to_bee_error(&e)))?;
    let ctx = FilterContext {
        me: Some(me_login(caller)),
        today: Some(bee_search::forge::CivilDate::from_days(
            (now_ms / 86_400_000) as i64,
        )),
    };
    q.to_filter(&ctx).map_err(ApiError::from)
}

/// Splits a forge query into the repo-selecting terms (`repo:`, `org:`, `user:`) and the
/// rest, by the parser's spans.
/// Repo-selecting terms of a query: `(key, value, negated)`.
type ScopeTerms = Vec<(String, String, bool)>;

fn split_scope(kind: ForgeKind, query: &str) -> ApiResult<(ScopeTerms, String)> {
    let parsed = bee_search::parse_search_query(QueryKind::Forge(kind), query);
    if !parsed.valid {
        return Err(ApiError::from(bee_search::query::to_bee_error(
            &parsed.errors,
        )));
    }
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut scope = Vec::new();
    for q in &parsed.qualifiers {
        if matches!(q.key.as_str(), "repo" | "org" | "user") {
            scope.push((q.key.clone(), q.resolved.clone(), q.negated));
            let (mut a, b) = (q.span.start as usize, q.span.end as usize);
            // Take a leading `-` of a negated term with it.
            if q.negated && a > 0 && query.as_bytes().get(a - 1) == Some(&b'-') {
                a -= 1;
            }
            cuts.push((a, b));
        }
    }
    let mut rest = String::new();
    let mut at = 0;
    cuts.sort();
    for (a, b) in cuts {
        if a >= at {
            rest.push_str(&query[at..a]);
            at = b;
        }
    }
    rest.push_str(&query[at.min(query.len())..]);
    Ok((scope, rest.split_whitespace().collect::<Vec<_>>().join(" ")))
}

fn scope_matches(scope: &[(String, String, bool)], repo: &RepoCtx) -> bool {
    let mut any_positive_repo = false;
    let mut matched_positive = false;
    for (k, v, neg) in scope {
        let hit = match k.as_str() {
            "repo" => {
                if v.contains('/') {
                    repo.full_name.eq_ignore_ascii_case(v)
                } else {
                    repo.repo.name.eq_ignore_ascii_case(v)
                }
            }
            _ => repo.org.name.eq_ignore_ascii_case(v),
        };
        if *neg {
            if hit {
                return false;
            }
        } else {
            any_positive_repo = true;
            matched_positive |= hit;
        }
    }
    !any_positive_repo || matched_positive
}

fn sub<T>(md: &MetadataMap, msg: T) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    *r.metadata_mut() = md.clone();
    r
}

fn principal_login(p: &Option<v1::Principal>) -> String {
    p.as_ref()
        .map(|p| {
            if p.display.is_empty() {
                p.id.clone()
            } else {
                p.display.clone()
            }
        })
        .unwrap_or_default()
}

fn sort_key(sort: &str, created: i64, updated: i64, comments: u32) -> i64 {
    match sort {
        "created" => created,
        "comments" => i64::from(comments),
        _ => updated,
    }
}

impl SearchApi {
    fn forge(&self) -> ApiResult<Arc<crate::forge::ForgeWiring>> {
        self.state
            .hub
            .get()
            .cloned()
            .ok_or_else(|| unavailable("the forge is not running on this server"))
    }

    /// Repos the caller may read that match the query's `repo:`/`org:`/`user:` terms.
    async fn forge_scope(
        &self,
        caller: &Caller,
        scope: &[(String, String, bool)],
    ) -> ApiResult<Vec<RepoCtx>> {
        let mut out = Vec::new();
        for r in all_repos(&self.state).await? {
            if out.len() >= MAX_REPOS {
                break;
            }
            if !scope_matches(scope, &r) {
                continue;
            }
            if self
                .state
                .gate(caller, &r, Action::Read, None)
                .await
                .is_ok()
            {
                out.push(r);
            }
        }
        Ok(out)
    }

    async fn search_issues_impl(
        &self,
        caller: &Caller,
        md: &MetadataMap,
        r: v1::SearchIssuesRequest,
    ) -> ApiResult<v1::SearchIssuesResponse> {
        use bee_proto::v1::change_service_server::ChangeService;
        use bee_proto::v1::issue_service_server::IssueService;
        let w = self.forge()?;
        let compiled = compile(ForgeKind::Issues, &r.query, caller, self.state.now_ms())?;
        // Which item types the query asks for.
        let mut want_issues = true;
        let mut want_crs = true;
        type_selection(&compiled.filter, &mut want_issues, &mut want_crs);
        let (scope, rest) = split_scope(ForgeKind::Issues, &r.query)?;
        let rest = strip_type_terms(&rest);
        let base = bee_forge::service::ForgeServices::new(w.forge.clone());
        let changes =
            bee_forge::changes::service::ChangeServices::new(w.forge.clone(), w.changes.clone());
        let mut hits: Vec<(i64, v1::SearchIssueHit)> = Vec::new();
        let mut incomplete = false;
        for repo in self.forge_scope(caller, &scope).await? {
            if want_issues {
                let mut token = String::new();
                let mut n = 0usize;
                loop {
                    let req = v1::ListIssuesRequest {
                        repo: repo_ref(&repo.full_name),
                        all_states: true,
                        query: rest.clone(),
                        page: Some(v1::PageRequest {
                            page_size: 200,
                            page_token: token.clone(),
                        }),
                        ..Default::default()
                    };
                    let Ok(resp) = base.list_issues(sub(md, req)).await else {
                        break;
                    };
                    let resp = resp.into_inner();
                    for i in resp.issues {
                        n += 1;
                        let key = sort_key(
                            &r.sort,
                            ts_ms(&i.created_at),
                            ts_ms(&i.updated_at),
                            i.comment_count,
                        );
                        hits.push((key, issue_hit(&repo, i, r.include_text_matches, &r.query)));
                    }
                    token = resp.page.map(|p| p.next_page_token).unwrap_or_default();
                    if token.is_empty() {
                        break;
                    }
                    if n >= MAX_PER_REPO {
                        incomplete = true;
                        break;
                    }
                }
            }
            if want_crs {
                let mut token = String::new();
                let mut n = 0usize;
                loop {
                    let req = v1::ListChangesRequest {
                        repo: repo_ref(&repo.full_name),
                        query: rest.clone(),
                        page: Some(v1::PageRequest {
                            page_size: 200,
                            page_token: token.clone(),
                        }),
                        ..Default::default()
                    };
                    let Ok(resp) = changes.list_changes(sub(md, req)).await else {
                        break;
                    };
                    let resp = resp.into_inner();
                    for c in resp.changes {
                        n += 1;
                        let key = sort_key(
                            &r.sort,
                            ts_ms(&c.created_at),
                            ts_ms(&c.updated_at),
                            c.comment_count,
                        );
                        hits.push((key, cr_hit(&repo, c)));
                    }
                    token = resp.page.map(|p| p.next_page_token).unwrap_or_default();
                    if token.is_empty() {
                        break;
                    }
                    if n >= MAX_PER_REPO {
                        incomplete = true;
                        break;
                    }
                }
            }
        }
        let asc = r.order == v1::SearchSortOrder::Asc as i32;
        hits.sort_by(|a, b| {
            let o = a.0.cmp(&b.0);
            let o = if asc { o } else { o.reverse() };
            o.then(a.1.repo_full_name.cmp(&b.1.repo_full_name))
                .then(a.1.number.cmp(&b.1.number))
        });
        let all: Vec<v1::SearchIssueHit> = hits.into_iter().map(|(_, h)| h).collect();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        Ok(v1::SearchIssuesResponse {
            total_count: all.len() as u64,
            hits: page,
            incomplete,
            page: next_page(next),
        })
    }
}

/// Narrows the item types from `type:`/`is:` terms at the top level of the filter.
fn type_selection(f: &Filter, issues: &mut bool, crs: &mut bool) {
    let mut check = |p: &Predicate| {
        if let Predicate::Enum { field, value } = p
            && *field == "type"
        {
            match *value {
                "issue" => *crs = false,
                "cr" | "pr" => *issues = false,
                _ => {}
            }
        }
    };
    match f {
        Filter::Pred(p) => check(p),
        Filter::And(v) => {
            for x in v {
                if let Filter::Pred(p) = x {
                    check(p);
                }
            }
        }
        _ => {}
    }
}

/// Drops `is:issue`, `is:pr`, `is:cr` and `type:…` terms (the per-type lists select those).
fn strip_type_terms(q: &str) -> String {
    q.split_whitespace()
        .filter(|t| {
            let l = t.to_ascii_lowercase();
            !(l == "is:issue" || l == "is:pr" || l == "is:cr" || l.starts_with("type:"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn text_matches(fields: &[(&str, &str)], query_text: &[String]) -> Vec<v1::SearchTextMatch> {
    let mut out = Vec::new();
    for (name, value) in fields {
        let lower = value.to_lowercase();
        let mut ranges = Vec::new();
        for t in query_text {
            let tl = t.to_lowercase();
            if tl.is_empty() {
                continue;
            }
            let mut from = 0;
            while let Some(i) = lower[from..].find(&tl) {
                let s = from + i;
                ranges.push(v1::SearchRange {
                    start: s as u32,
                    end: (s + tl.len()) as u32,
                });
                from = s + tl.len();
            }
        }
        if !ranges.is_empty() {
            out.push(v1::SearchTextMatch {
                field: (*name).to_string(),
                fragment: (*value).to_string(),
                ranges,
            });
        }
    }
    out
}

/// Free-text words of a query (terms without a qualifier).
fn free_text(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| !t.contains(':') && !t.starts_with('-'))
        .map(|t| t.trim_matches('"').to_string())
        .filter(|t| !t.is_empty() && t != "AND" && t != "OR" && t != "NOT")
        .collect()
}

fn issue_hit(repo: &RepoCtx, i: v1::Issue, matches: bool, query: &str) -> v1::SearchIssueHit {
    let state = if i.state == v1::IssueState::Closed as i32 {
        "closed"
    } else {
        "open"
    };
    let reason = match v1::IssueStateReason::try_from(i.state_reason) {
        Ok(v1::IssueStateReason::Completed) => "completed",
        Ok(v1::IssueStateReason::NotPlanned) => "not_planned",
        Ok(v1::IssueStateReason::Duplicate) => "duplicate",
        _ => "",
    };
    let text = if matches {
        text_matches(
            &[("title", i.title.as_str()), ("body", i.body.as_str())],
            &free_text(query),
        )
    } else {
        Vec::new()
    };
    v1::SearchIssueHit {
        subject: Some(v1::SubjectRef {
            kind: v1::SubjectKind::Issue as i32,
            repo: repo_ref(&repo.full_name),
            id: i.id.clone(),
        }),
        number: i.number,
        title: i.title,
        state: state.into(),
        state_reason: reason.into(),
        author: i.author,
        assignees: i.assignees,
        labels: i.labels,
        milestone: i.milestone.map(|m| m.title).unwrap_or_default(),
        comment_count: i.comment_count,
        created_at: i.created_at,
        updated_at: i.updated_at,
        closed_at: i.closed_at,
        repo_full_name: repo.full_name.clone(),
        text_matches: text,
        url: i.url,
        ..Default::default()
    }
}

fn cr_hit(repo: &RepoCtx, c: v1::ChangeRequest) -> v1::SearchIssueHit {
    let state = match v1::ChangeState::try_from(c.state) {
        Ok(v1::ChangeState::Landed) => "landed",
        Ok(v1::ChangeState::Closed) => "closed",
        _ if c.draft => "draft",
        _ => "open",
    };
    v1::SearchIssueHit {
        subject: Some(v1::SubjectRef {
            kind: v1::SubjectKind::ChangeRequest as i32,
            repo: repo_ref(&repo.full_name),
            id: c.id.clone(),
        }),
        number: c.number,
        title: c.title,
        state: state.into(),
        author: c.author,
        assignees: c.assignees,
        labels: c.labels,
        milestone: c.milestone.map(|m| m.title).unwrap_or_default(),
        comment_count: c.comment_count,
        base_ref: c.target_ref,
        head_ref: format!("cr/{}", c.number),
        created_at: c.created_at,
        updated_at: c.updated_at,
        closed_at: c.closed_at.or(c.landed_at),
        repo_full_name: repo.full_name.clone(),
        ..Default::default()
    }
}

/// A repo's forge record (or a bare one when the forge has none).
async fn forge_repo(state: &AppState, repo: &RepoCtx) -> v1::Repo {
    if let Some(w) = state.hub.get()
        && let Ok(mut r) = w
            .forge
            .orgs
            .repo_any(&bee_forge::Cx::system(), &repo.id_text())
            .await
    {
        // The repo record never carries its star count: stars are a community counter, kept
        // where starring writes them. Without this every hit reads 0 stars and sorting by
        // stars falls back to the name order.
        if let Ok(n) = w
            .forge
            .community
            .users
            .star_count(&bee_forge::Cx::system(), &r.id)
            .await
        {
            r.stars = n;
        }
        return r;
    }
    v1::Repo {
        id: repo.id_text(),
        org_id: repo.org.id.0.to_text(),
        name: repo.repo.name.clone(),
        full_name: repo.full_name.clone(),
        default_ref: "main".into(),
        ..Default::default()
    }
}

fn visibility_name(v: i32) -> &'static str {
    match v1::Visibility::try_from(v) {
        Ok(v1::Visibility::Public) => "public",
        Ok(v1::Visibility::Internal) => "internal",
        _ => "private",
    }
}

fn repo_hit(r: &v1::Repo, matches: Vec<v1::SearchTextMatch>, score: f32) -> v1::SearchRepoHit {
    v1::SearchRepoHit {
        repo: repo_ref(&r.full_name),
        full_name: r.full_name.clone(),
        description: r.description.clone(),
        topics: r.topics.clone(),
        primary_language: r.primary_language.clone(),
        star_count: r.stars,
        fork_count: r.forks,
        open_issue_count: r.open_issues,
        license_spdx_id: r.license_spdx.clone(),
        archived: r.archived,
        is_fork: !r.fork_parent_id.is_empty(),
        is_template: r.is_template,
        visibility: visibility_name(r.visibility).into(),
        updated_at: r.pushed_at.or(r.created_at),
        pushed_at: r.pushed_at,
        text_matches: matches,
        score,
    }
}

fn repo_matches(r: &v1::Repo, p: &Predicate) -> bool {
    use bee_search::forge::Key;
    match p {
        Predicate::Text { text, fields } => fields.iter().any(|f| match *f {
            "name" => contains_ci(&r.name, text) || contains_ci(&r.full_name, text),
            "description" => contains_ci(&r.description, text),
            "topics" => r.topics.iter().any(|t| contains_ci(t, text)),
            _ => false,
        }),
        Predicate::Name { field, value } => match field {
            Key::Topic => r.topics.iter().any(|t| t.eq_ignore_ascii_case(value)),
            Key::Language => r.primary_language.eq_ignore_ascii_case(value),
            Key::License => r.license_spdx.eq_ignore_ascii_case(value),
            Key::Repo => {
                r.full_name.eq_ignore_ascii_case(value) || r.name.eq_ignore_ascii_case(value)
            }
            Key::Org | Key::User => r
                .full_name
                .split('/')
                .next()
                .is_some_and(|o| o.eq_ignore_ascii_case(value)),
            _ => false,
        },
        Predicate::Enum { field, value } => match (*field, *value) {
            ("visibility", v) => visibility_name(r.visibility) == v,
            ("fork", "true") | ("fork", "only") => !r.fork_parent_id.is_empty(),
            ("fork", "false") => r.fork_parent_id.is_empty(),
            ("type", "fork") => !r.fork_parent_id.is_empty(),
            _ => false,
        },
        Predicate::Flag { field, value } => match *field {
            "archived" => r.archived == *value,
            "template" => r.is_template == *value,
            "fork" => r.fork_parent_id.is_empty() != *value,
            "mirror" | "sponsorable" => !*value,
            _ => false,
        },
        Predicate::Range { field, min, max } => {
            let v: i64 = match field {
                Key::Stars => r.stars as i64,
                Key::Forks => r.forks as i64,
                Key::Size => (r.size_bytes / 1024) as i64,
                Key::Topics => r.topics.len() as i64,
                Key::Created => ts_ms(&r.created_at),
                Key::Pushed => ts_ms(&r.pushed_at),
                _ => return false,
            };
            in_range(v, min, max)
        }
        Predicate::Missing(_) | Predicate::Person { .. } | Predicate::AgentAuthor { .. } => false,
        Predicate::PathPrefix(_) => false,
    }
}

impl SearchApi {
    async fn readable_repos(&self, caller: &Caller) -> ApiResult<Vec<(RepoCtx, v1::Repo)>> {
        let mut out = Vec::new();
        for r in all_repos(&self.state).await?.into_iter().take(MAX_REPOS) {
            if self
                .state
                .gate(caller, &r, Action::Read, None)
                .await
                .is_ok()
            {
                let f = forge_repo(&self.state, &r).await;
                if f.deleted_at.is_none() {
                    out.push((r, f));
                }
            }
        }
        Ok(out)
    }

    async fn search_repos_impl(
        &self,
        caller: &Caller,
        r: v1::SearchReposRequest,
    ) -> ApiResult<v1::SearchReposResponse> {
        let c = compile(ForgeKind::Repos, &r.query, caller, self.state.now_ms())?;
        let words = free_text(&r.query);
        let mut hits: Vec<v1::SearchRepoHit> = Vec::new();
        for (_, f) in self.readable_repos(caller).await? {
            if !eval(&c.filter, &|p| repo_matches(&f, p)) {
                continue;
            }
            let tm = if r.include_text_matches {
                text_matches(
                    &[
                        ("name", f.full_name.as_str()),
                        ("description", f.description.as_str()),
                    ],
                    &words,
                )
            } else {
                Vec::new()
            };
            let score = words.iter().filter(|w| contains_ci(&f.name, w)).count() as f32 * 10.0
                + f.stars as f32 / 1000.0;
            hits.push(repo_hit(&f, tm, score));
        }
        let field = if r.sort.is_empty() {
            c.sort.field.to_string()
        } else {
            r.sort.clone()
        };
        let asc = r.order == v1::SearchSortOrder::Asc as i32;
        hits.sort_by(|a, b| {
            let o = match field.as_str() {
                "stars" => a.star_count.cmp(&b.star_count),
                "forks" => a.fork_count.cmp(&b.fork_count),
                "updated" => ts_ms(&a.updated_at).cmp(&ts_ms(&b.updated_at)),
                "help-wanted-issues" => a.open_issue_count.cmp(&b.open_issue_count),
                _ => a
                    .score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            let o = if asc { o } else { o.reverse() };
            o.then(a.full_name.cmp(&b.full_name))
        });
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&hits, &token, size)?;
        Ok(v1::SearchReposResponse {
            total_count: hits.len() as u64,
            hits: page,
            incomplete: false,
            page: next_page(next),
        })
    }

    async fn search_changesets_impl(
        &self,
        caller: &Caller,
        r: v1::SearchChangesetsRequest,
    ) -> ApiResult<v1::SearchChangesetsResponse> {
        use bee_search::forge::Key;
        let c = compile(ForgeKind::Changesets, &r.query, caller, self.state.now_ms())?;
        let (scope, _) = split_scope(ForgeKind::Changesets, &r.query)?;
        let words = free_text(&r.query);
        let mut hits: Vec<(i64, v1::SearchChangesetHit)> = Vec::new();
        let mut incomplete = false;
        for repo in self.forge_scope(caller, &scope).await? {
            let history = self.state.history(&repo);
            let Ok(tip) = crate::services::query::resolve_rev(&history, None).await else {
                continue;
            };
            let mut token = None;
            let mut seen = 0usize;
            loop {
                let page = history
                    .log(
                        &tip,
                        &LogOptions {
                            limit: 200,
                            page_token: token.clone(),
                            ..LogOptions::default()
                        },
                    )
                    .await?;
                for e in &page.entries {
                    seen += 1;
                    let Some(m) = &e.meta else { continue };
                    if e.parents.is_empty() {
                        continue;
                    }
                    let id_text = e.id.to_text();
                    let author_time = m.author_time.seconds * 1000;
                    let committer_time = m
                        .committer_time
                        .map(|t| t.seconds * 1000)
                        .unwrap_or(author_time);
                    let committer = m.committer.clone().unwrap_or_else(|| m.author.clone());
                    let ok = eval(&c.filter, &|p| match p {
                        Predicate::Text { text, .. } => contains_ci(&m.message, text),
                        Predicate::Person { role, login } => match role {
                            Key::Committer => contains_ci(&committer, login),
                            _ => contains_ci(&m.author, login) || m.creator == *login,
                        },
                        Predicate::Name { field, value } => match field {
                            Key::Hash => id_text.to_lowercase().starts_with(&value.to_lowercase()),
                            Key::AuthorName | Key::AuthorEmail => contains_ci(&m.author, value),
                            Key::CommitterName | Key::CommitterEmail => {
                                contains_ci(&committer, value)
                            }
                            Key::Parent => e.parents.iter().any(|p| {
                                p.to_text()
                                    .to_lowercase()
                                    .starts_with(&value.to_lowercase())
                            }),
                            Key::Repo => {
                                repo.full_name.eq_ignore_ascii_case(value)
                                    || repo.repo.name.eq_ignore_ascii_case(value)
                            }
                            Key::Org | Key::User => repo.org.name.eq_ignore_ascii_case(value),
                            _ => false,
                        },
                        Predicate::Range { field, min, max } => match field {
                            Key::AuthorDate => in_range(author_time, min, max),
                            Key::CommitterDate => in_range(committer_time, min, max),
                            _ => false,
                        },
                        Predicate::Flag { field, value } => {
                            *field == "merge" && (e.parents.len() > 1) == *value
                        }
                        Predicate::AgentAuthor { is_agent, .. } => {
                            m.provenance.is_some() == *is_agent
                        }
                        _ => false,
                    });
                    if !ok {
                        continue;
                    }
                    let tm = if r.include_text_matches {
                        text_matches(&[("message", m.message.as_str())], &words)
                    } else {
                        Vec::new()
                    };
                    let key = if r.sort == "committer-date" {
                        committer_time
                    } else {
                        author_time
                    };
                    hits.push((
                        key,
                        v1::SearchChangesetHit {
                            repo: repo_ref(&repo.full_name),
                            repo_full_name: repo.full_name.clone(),
                            changeset: Some(id_to(&e.id)),
                            change_id: Some(v1::ChangeId {
                                raw: e.change_id.as_bytes().to_vec(),
                            }),
                            message: m.message.clone(),
                            author: Some(v1::Principal {
                                id: m.creator.clone(),
                                display: m.author.clone(),
                                ..Default::default()
                            }),
                            committer: Some(v1::Principal {
                                display: committer.clone(),
                                ..Default::default()
                            }),
                            authored_at: Some(timestamp_ms(author_time.max(0) as u64)),
                            committed_at: Some(timestamp_ms(committer_time.max(0) as u64)),
                            text_matches: tm,
                        },
                    ));
                }
                match page.next_page_token {
                    Some(t) if seen < MAX_PER_REPO => token = Some(t),
                    Some(_) => {
                        incomplete = true;
                        break;
                    }
                    None => break,
                }
            }
        }
        let asc = r.order == v1::SearchSortOrder::Asc as i32;
        hits.sort_by(|a, b| {
            let o = a.0.cmp(&b.0);
            let o = if asc { o } else { o.reverse() };
            o.then(a.1.repo_full_name.cmp(&b.1.repo_full_name))
        });
        let all: Vec<_> = hits.into_iter().map(|(_, h)| h).collect();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        Ok(v1::SearchChangesetsResponse {
            total_count: all.len() as u64,
            hits: page,
            incomplete,
            page: next_page(next),
        })
    }

    async fn search_users_impl(
        &self,
        caller: &Caller,
        r: v1::SearchUsersRequest,
    ) -> ApiResult<v1::SearchUsersResponse> {
        use bee_search::forge::Key;
        let c = compile(ForgeKind::Users, &r.query, caller, self.state.now_ms())?;
        let mut hits: Vec<v1::SearchUserHit> = Vec::new();
        let principals = scan_json::<crate::auth::PrincipalRecord>(
            self.state.kv.as_ref(),
            crate::auth::NS_PRINCIPAL,
            "",
        )
        .await?;
        for (_, p) in principals {
            if p.deactivated {
                continue;
            }
            let created = bee_core::Ulid::parse(&p.id)
                .map(|u| u.timestamp_ms() as i64)
                .unwrap_or(0);
            let kind = p.kind.clone();
            let ok = eval(&c.filter, &|pr| match pr {
                Predicate::Text { text, fields } => fields.iter().any(|f| match *f {
                    "login" | "name" => contains_ci(&p.display, text) || contains_ci(&p.id, text),
                    _ => false,
                }),
                Predicate::Enum { field, value } => {
                    *field == "type"
                        && match *value {
                            "user" => kind == "human",
                            "agent" => kind == "agent",
                            "org" => false,
                            _ => false,
                        }
                }
                Predicate::Range { field, min, max } => {
                    matches!(field, Key::Created) && in_range(created, min, max)
                }
                _ => false,
            });
            if ok {
                hits.push(v1::SearchUserHit {
                    principal: Some(crate::services::auth::principal_to(&p)),
                    login: p.display.clone(),
                    name: p.display.clone(),
                    ..Default::default()
                });
            }
        }
        // Orgs are accounts too (`type:org`).
        for org in crate::services::support::all_orgs(&self.state).await? {
            let ok = eval(&c.filter, &|pr| match pr {
                Predicate::Text { text, .. } => contains_ci(&org.name, text),
                Predicate::Enum { field, value } => *field == "type" && *value == "org",
                Predicate::Range { field, min, max } => {
                    matches!(field, Key::Created) && in_range(org.created_at_ms as i64, min, max)
                }
                _ => false,
            });
            if ok {
                hits.push(v1::SearchUserHit {
                    principal: Some(v1::Principal {
                        id: org.id.0.to_text(),
                        display: org.name.clone(),
                        ..Default::default()
                    }),
                    login: org.name.clone(),
                    name: org.name.clone(),
                    is_org: true,
                    ..Default::default()
                });
            }
        }
        hits.sort_by_key(|h| h.login.to_lowercase());
        if r.order == v1::SearchSortOrder::Desc as i32 && r.sort == "joined" {
            hits.reverse();
        }
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&hits, &token, size)?;
        Ok(v1::SearchUsersResponse {
            total_count: hits.len() as u64,
            hits: page,
            incomplete: false,
            page: next_page(next),
        })
    }

    /// Topics of the caller's readable repos: name → repos.
    async fn topics(&self, caller: &Caller) -> ApiResult<BTreeMap<String, Vec<v1::Repo>>> {
        let mut out: BTreeMap<String, Vec<v1::Repo>> = BTreeMap::new();
        for (_, f) in self.readable_repos(caller).await? {
            for t in &f.topics {
                out.entry(t.to_lowercase()).or_default().push(f.clone());
            }
        }
        Ok(out)
    }

    fn topic_proto(&self, name: &str, repos: &[v1::Repo], top: usize) -> v1::Topic {
        let mut sorted = repos.to_vec();
        sorted.sort_by(|a, b| b.stars.cmp(&a.stars).then(a.full_name.cmp(&b.full_name)));
        let featured = self
            .state
            .cfg
            .search
            .featured_topics
            .iter()
            .any(|t| t.eq_ignore_ascii_case(name));
        v1::Topic {
            name: name.to_string(),
            display_name: name.to_string(),
            featured,
            curated: featured,
            repo_count: repos.len() as u64,
            top_repos: sorted
                .iter()
                .take(top)
                .map(|r| repo_hit(r, Vec::new(), 0.0))
                .collect(),
            ..Default::default()
        }
    }

    async fn search_topics_impl(
        &self,
        caller: &Caller,
        r: v1::SearchTopicsRequest,
    ) -> ApiResult<v1::SearchTopicsResponse> {
        use bee_search::forge::Key;
        let c = compile(ForgeKind::Topics, &r.query, caller, self.state.now_ms())?;
        let mut out = Vec::new();
        for (name, repos) in self.topics(caller).await? {
            let featured = self
                .state
                .cfg
                .search
                .featured_topics
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&name));
            let ok = eval(&c.filter, &|p| match p {
                Predicate::Text { text, .. } => contains_ci(&name, text),
                Predicate::Enum { field, .. } | Predicate::Flag { field, .. }
                    if *field == "featured" || *field == "curated" =>
                {
                    featured
                }
                Predicate::Range { field, min, max } => {
                    matches!(field, Key::Repositories) && in_range(repos.len() as i64, min, max)
                }
                _ => false,
            });
            if ok {
                out.push(self.topic_proto(&name, &repos, 0));
            }
        }
        out.sort_by(|a, b| b.repo_count.cmp(&a.repo_count).then(a.name.cmp(&b.name)));
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&out, &token, size)?;
        Ok(v1::SearchTopicsResponse {
            total_count: out.len() as u64,
            topics: page,
            page: next_page(next),
        })
    }

    async fn search_discussions_impl(
        &self,
        caller: &Caller,
        md: &MetadataMap,
        r: v1::SearchDiscussionsRequest,
    ) -> ApiResult<v1::SearchDiscussionsResponse> {
        use bee_proto::v1::discussion_service_server::DiscussionService;
        use bee_search::forge::Key;
        let w = self.forge()?;
        let c = compile(
            ForgeKind::Discussions,
            &r.query,
            caller,
            self.state.now_ms(),
        )?;
        let (scope, _) = split_scope(ForgeKind::Discussions, &r.query)?;
        let base = bee_forge::service::ForgeServices::new(w.forge.clone());
        let words = free_text(&r.query);
        let mut hits: Vec<(i64, v1::SearchDiscussionHit)> = Vec::new();
        for repo in self.forge_scope(caller, &scope).await? {
            let mut token = String::new();
            loop {
                let req = v1::ListDiscussionsRequest {
                    repo: repo_ref(&repo.full_name),
                    include_closed: true,
                    page: Some(v1::PageRequest {
                        page_size: 200,
                        page_token: token.clone(),
                    }),
                    ..Default::default()
                };
                let Ok(resp) = base.list_discussions(sub(md, req)).await else {
                    break;
                };
                let resp = resp.into_inner();
                for d in resp.discussions {
                    let author = principal_login(&d.author);
                    let category = d
                        .category
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let answered = !d.answer_comment_id.is_empty();
                    let ok = eval(&c.filter, &|p| match p {
                        Predicate::Text { text, fields } => fields.iter().any(|f| match *f {
                            "title" => contains_ci(&d.title, text),
                            "body" => contains_ci(&d.body, text),
                            _ => false,
                        }),
                        Predicate::Person { login, .. } => author.eq_ignore_ascii_case(login),
                        Predicate::Name { field, value } => match field {
                            Key::Category => category.eq_ignore_ascii_case(value),
                            Key::Label => {
                                d.labels.iter().any(|l| l.name.eq_ignore_ascii_case(value))
                            }
                            // Scope qualifiers already chose the repos; they hold for each.
                            Key::Repo => {
                                repo.full_name.eq_ignore_ascii_case(value)
                                    || repo.repo.name.eq_ignore_ascii_case(value)
                            }
                            Key::Org | Key::User => repo.org.name.eq_ignore_ascii_case(value),
                            _ => false,
                        },
                        Predicate::Enum { field, value } => match (*field, *value) {
                            ("answered", "answered") => answered,
                            ("answered", "unanswered") => !answered,
                            ("state", "open") => !d.closed,
                            ("state", "closed") => d.closed,
                            _ => false,
                        },
                        Predicate::Flag { field, value } => {
                            *field == "locked" && d.locked == *value
                        }
                        Predicate::Range { field, min, max } => match field {
                            Key::Created => in_range(ts_ms(&d.created_at), min, max),
                            Key::Updated => in_range(ts_ms(&d.updated_at), min, max),
                            Key::Comments => in_range(i64::from(d.comment_count), min, max),
                            _ => false,
                        },
                        _ => false,
                    });
                    if !ok {
                        continue;
                    }
                    let key = sort_key(
                        &r.sort,
                        ts_ms(&d.created_at),
                        ts_ms(&d.updated_at),
                        d.comment_count,
                    );
                    let tm = if r.include_text_matches {
                        text_matches(
                            &[("title", d.title.as_str()), ("body", d.body.as_str())],
                            &words,
                        )
                    } else {
                        Vec::new()
                    };
                    hits.push((
                        key,
                        v1::SearchDiscussionHit {
                            subject: Some(v1::SubjectRef {
                                kind: v1::SubjectKind::Discussion as i32,
                                repo: repo_ref(&repo.full_name),
                                id: d.id.clone(),
                            }),
                            number: d.number,
                            title: d.title.clone(),
                            category,
                            author: d.author.clone(),
                            answered,
                            comment_count: d.comment_count,
                            created_at: d.created_at,
                            updated_at: d.updated_at,
                            repo_full_name: repo.full_name.clone(),
                            text_matches: tm,
                        },
                    ));
                }
                token = resp.page.map(|p| p.next_page_token).unwrap_or_default();
                if token.is_empty() {
                    break;
                }
            }
        }
        let asc = r.order == v1::SearchSortOrder::Asc as i32;
        hits.sort_by(|a, b| {
            let o = a.0.cmp(&b.0);
            let o = if asc { o } else { o.reverse() };
            o.then(a.1.repo_full_name.cmp(&b.1.repo_full_name))
                .then(a.1.number.cmp(&b.1.number))
        });
        let all: Vec<_> = hits.into_iter().map(|(_, h)| h).collect();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        Ok(v1::SearchDiscussionsResponse {
            total_count: all.len() as u64,
            hits: page,
            incomplete: false,
            page: next_page(next),
        })
    }

    async fn search_packages_impl(
        &self,
        caller: &Caller,
        md: &MetadataMap,
        r: v1::SearchPackagesRequest,
    ) -> ApiResult<v1::SearchPackagesResponse> {
        use bee_proto::v1::package_service_server::PackageService;
        use bee_search::forge::Key;
        let w = self.forge()?;
        let c = compile(ForgeKind::Packages, &r.query, caller, self.state.now_ms())?;
        let base = bee_forge::service::ForgeServices::new(w.forge.clone());
        let mut hits: Vec<v1::SearchPackageHit> = Vec::new();
        for org in crate::services::support::all_orgs(&self.state).await? {
            let mut token = String::new();
            loop {
                let req = v1::ListPackagesRequest {
                    owner: org.name.clone(),
                    page: Some(v1::PageRequest {
                        page_size: 200,
                        page_token: token.clone(),
                    }),
                    ..Default::default()
                };
                let Ok(resp) = base.list_packages(sub(md, req)).await else {
                    break;
                };
                let resp = resp.into_inner();
                for p in resp.packages {
                    let eco = v1::PackageEcosystem::try_from(p.ecosystem)
                        .map(|e| {
                            e.as_str_name()
                                .trim_start_matches("PACKAGE_ECOSYSTEM_")
                                .to_lowercase()
                        })
                        .unwrap_or_default();
                    let repo_name = match p.repo.as_ref().and_then(|r| r.repo.as_ref()) {
                        Some(v1::repo_ref::Repo::FullName(n)) => n.clone(),
                        Some(v1::repo_ref::Repo::Id(i)) => i.clone(),
                        None => String::new(),
                    };
                    let ok = eval(&c.filter, &|pr| match pr {
                        Predicate::Text { text, fields } => fields.iter().any(|f| match *f {
                            "name" => contains_ci(&p.name, text),
                            "description" => contains_ci(&p.description, text),
                            _ => false,
                        }),
                        Predicate::Enum { field, value } => *field == "type" && eco == *value,
                        Predicate::Name { field, value } => match field {
                            Key::Org | Key::User => p.owner.eq_ignore_ascii_case(value),
                            Key::Repo => repo_name.eq_ignore_ascii_case(value),
                            _ => false,
                        },
                        _ => false,
                    });
                    if ok {
                        hits.push(v1::SearchPackageHit {
                            subject: Some(v1::SubjectRef {
                                kind: v1::SubjectKind::Package as i32,
                                repo: None,
                                id: p.id.clone(),
                            }),
                            name: p.name.clone(),
                            ecosystem: eco,
                            owner: p.owner.clone(),
                            repo_full_name: repo_name,
                            description: p.description.clone(),
                            download_count: p.download_count,
                            updated_at: p.updated_at,
                            ..Default::default()
                        });
                    }
                }
                token = resp.page.map(|p| p.next_page_token).unwrap_or_default();
                if token.is_empty() {
                    break;
                }
            }
        }
        let asc = r.order == v1::SearchSortOrder::Asc as i32;
        hits.sort_by(|a, b| {
            let o = match r.sort.as_str() {
                "downloads" => a.download_count.cmp(&b.download_count),
                "updated" => ts_ms(&a.updated_at).cmp(&ts_ms(&b.updated_at)),
                _ => a.name.cmp(&b.name).reverse(),
            };
            let o = if asc { o } else { o.reverse() };
            o.then(a.owner.cmp(&b.owner)).then(a.name.cmp(&b.name))
        });
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&hits, &token, size)?;
        Ok(v1::SearchPackagesResponse {
            total_count: hits.len() as u64,
            hits: page,
            page: next_page(next),
        })
    }
}

/// One developer's lands in a window: count, display name, and per-repo counts.
type DeveloperActivity = (u64, String, BTreeMap<String, (u64, v1::Repo)>);

/// The (subject, metric) of a leaderboard request, defaults applied (agents, lands);
/// `INVALID_ARGUMENT` for unknown values and for combinations that count nothing.
fn leaderboard_kind(
    subject: i32,
    metric: i32,
) -> ApiResult<(v1::LeaderboardSubject, v1::LeaderboardMetric)> {
    use v1::{LeaderboardMetric as M, LeaderboardSubject as S};
    let subject = match S::try_from(subject) {
        Ok(S::Unspecified) => S::Agents,
        Ok(s) => s,
        Err(_) => {
            return Err(ApiError::invalid(
                "invalid_field",
                format!("subject {subject} is not a LeaderboardSubject"),
            ));
        }
    };
    let metric = match M::try_from(metric) {
        Ok(M::Unspecified) => M::Lands,
        Ok(m) => m,
        Err(_) => {
            return Err(ApiError::invalid(
                "invalid_field",
                format!("metric {metric} is not a LeaderboardMetric"),
            ));
        }
    };
    match (subject, metric) {
        (S::Repos, M::Reviews) => Err(ApiError::invalid(
            "invalid_field",
            "metric REVIEWS ranks agents or humans, not repos",
        )),
        (S::Agents | S::Humans, M::Stars) => Err(ApiError::invalid(
            "invalid_field",
            "metric STARS ranks repos, not agents or humans",
        )),
        _ => Ok((subject, metric)),
    }
}

/// One leaderboard candidate: its score, the name it is ordered by on a tie, and its id (so
/// two candidates that share a name still order the same way every time).
struct LeaderRow {
    score: u64,
    name: String,
    id: String,
    entry: v1::LeaderboardEntry,
}

/// Ranks leaderboard rows: candidates that scored nothing are left out, then score
/// descending, name ascending, id ascending; ranks count from 1.
fn rank_leaders(mut rows: Vec<LeaderRow>) -> Vec<v1::LeaderboardEntry> {
    rows.retain(|r| r.score > 0);
    rows.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows.into_iter()
        .enumerate()
        .map(|(i, r)| {
            let mut e = r.entry;
            e.rank = i as u32 + 1;
            e.score = r.score;
            e
        })
        .collect()
}

// ----- saved searches, trending, topics -----------------------------------------------------

/// A stored saved search.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SavedRec {
    /// Id.
    pub id: String,
    /// Owner.
    pub principal: String,
    /// Name.
    pub name: String,
    /// `SearchQueryKind`.
    pub kind: i32,
    /// Normalized query.
    pub query: String,
    /// Sort.
    pub sort: String,
    /// `SearchSortOrder`.
    pub order: i32,
    /// Org scope.
    pub org: String,
    /// Repo scope (full name).
    pub repo: String,
    /// Pinned.
    pub pinned: bool,
    /// Created.
    pub created_ms: u64,
    /// Updated.
    pub updated_ms: u64,
}

fn saved_proto(r: &SavedRec) -> v1::SavedSearch {
    v1::SavedSearch {
        id: r.id.clone(),
        name: r.name.clone(),
        kind: r.kind,
        query: r.query.clone(),
        sort: r.sort.clone(),
        order: r.order,
        org: r.org.clone(),
        repo: (!r.repo.is_empty()).then(|| v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::FullName(r.repo.clone())),
        }),
        pinned: r.pinned,
        created_at: Some(timestamp_ms(r.created_ms)),
        updated_at: Some(timestamp_ms(r.updated_ms)),
    }
}

impl SearchApi {
    async fn saved(&self, caller: &Caller, id: &str) -> ApiResult<SavedRec> {
        if id.is_empty() {
            return Err(ApiError::invalid(
                "missing_field",
                "saved_search_id is required",
            ));
        }
        get_json(
            self.state.kv.as_ref(),
            NS_SAVED,
            &format!("{}/{id}", caller.id()),
        )
        .await?
        .ok_or_else(|| ApiError::not_found(format!("saved search {id}")))
    }

    async fn create_saved(
        &self,
        caller: &Caller,
        r: v1::CreateSavedSearchRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::SavedSearch> {
        let s = &self.state;
        let kind = query_kind(r.kind)?;
        let normalized =
            bee_search::validate_saved_search(kind, &r.name, &r.query).map_err(ApiError::from)?;
        let repo = match &r.repo {
            Some(rr) if rr.repo.is_some() => {
                let ctx = s.repo(Some(rr)).await?;
                s.gate(caller, &ctx, Action::Read, None).await?;
                ctx.full_name
            }
            _ => String::new(),
        };
        let now = s.now_ms();
        let rec = SavedRec {
            id: crate::util::new_ulid(now)?.to_text(),
            principal: caller.id().to_string(),
            name: r.name.trim().to_string(),
            kind: r.kind,
            query: normalized,
            sort: r.sort,
            order: r.order,
            org: r.org,
            repo,
            pinned: r.pinned,
            created_ms: now,
            updated_ms: now,
        };
        let name_key = format!("{}/{}", caller.id(), rec.name.to_lowercase());
        let ok =
            s.kv.apply(vec![
                crate::kv::KvOp::expect(NS_SAVED_NAME, name_key.clone().into_bytes(), None),
                crate::kv::KvOp::put(NS_SAVED_NAME, name_key.into_bytes(), rec.id.clone()),
                crate::kv::KvOp::put(
                    NS_SAVED,
                    format!("{}/{}", caller.id(), rec.id).into_bytes(),
                    serde_json::to_vec(&rec).unwrap_or_default(),
                ),
            ])
            .await?;
        if !ok {
            return Err(ApiError::new(
                ErrorReason::AlreadyExists,
                format!("a saved search named {:?} exists", rec.name),
            ));
        }
        s.audit(
            caller,
            meta,
            None,
            &rec.id,
            "saved_search.create",
            json!({}),
        )
        .await?;
        Ok(saved_proto(&rec))
    }

    async fn update_saved(
        &self,
        caller: &Caller,
        r: v1::UpdateSavedSearchRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::SavedSearch> {
        let s = &self.state;
        let mut rec = self.saved(caller, &r.saved_search_id).await?;
        let old_name = rec.name.clone();
        let kind = query_kind(rec.kind)?;
        if let Some(n) = &r.name {
            rec.name = n.trim().to_string();
        }
        let query = r.query.clone().unwrap_or_else(|| rec.query.clone());
        rec.query =
            bee_search::validate_saved_search(kind, &rec.name, &query).map_err(ApiError::from)?;
        if let Some(v) = r.sort {
            rec.sort = v;
        }
        if let Some(v) = r.order {
            rec.order = v;
        }
        if let Some(v) = r.pinned {
            rec.pinned = v;
        }
        rec.updated_ms = s.now_ms();
        let mut ops = vec![crate::kv::KvOp::put(
            NS_SAVED,
            format!("{}/{}", caller.id(), rec.id).into_bytes(),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )];
        if rec.name.to_lowercase() != old_name.to_lowercase() {
            let new_key = format!("{}/{}", caller.id(), rec.name.to_lowercase());
            ops.insert(
                0,
                crate::kv::KvOp::expect(NS_SAVED_NAME, new_key.clone().into_bytes(), None),
            );
            ops.push(crate::kv::KvOp::put(
                NS_SAVED_NAME,
                new_key.into_bytes(),
                rec.id.clone(),
            ));
            ops.push(crate::kv::KvOp::delete(
                NS_SAVED_NAME,
                format!("{}/{}", caller.id(), old_name.to_lowercase()).into_bytes(),
            ));
        }
        if !s.kv.apply(ops).await? {
            return Err(ApiError::new(
                ErrorReason::AlreadyExists,
                format!("a saved search named {:?} exists", rec.name),
            ));
        }
        s.audit(
            caller,
            meta,
            None,
            &rec.id,
            "saved_search.update",
            json!({}),
        )
        .await?;
        Ok(saved_proto(&rec))
    }

    async fn delete_saved(&self, caller: &Caller, id: &str, meta: &RequestMeta) -> ApiResult<()> {
        let s = &self.state;
        let rec = self.saved(caller, id).await?;
        s.kv.apply(vec![
            crate::kv::KvOp::delete(NS_SAVED, format!("{}/{}", caller.id(), rec.id).into_bytes()),
            crate::kv::KvOp::delete(
                NS_SAVED_NAME,
                format!("{}/{}", caller.id(), rec.name.to_lowercase()).into_bytes(),
            ),
        ])
        .await?;
        s.audit(
            caller,
            meta,
            None,
            &rec.id,
            "saved_search.delete",
            json!({}),
        )
        .await?;
        Ok(())
    }

    async fn list_saved(
        &self,
        caller: &Caller,
        r: v1::ListSavedSearchesRequest,
    ) -> ApiResult<v1::ListSavedSearchesResponse> {
        let mut all: Vec<SavedRec> = scan_json::<SavedRec>(
            self.state.kv.as_ref(),
            NS_SAVED,
            &format!("{}/", caller.id()),
        )
        .await?
        .into_iter()
        .map(|(_, r)| r)
        .filter(|x| r.kind == 0 || x.kind == r.kind)
        .collect();
        all.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let protos: Vec<v1::SavedSearch> = all.iter().map(saved_proto).collect();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&protos, &token, size)?;
        Ok(v1::ListSavedSearchesResponse {
            saved_searches: page,
            page: next_page(next),
        })
    }

    fn window_ms(w: i32) -> u64 {
        match v1::TrendingWindow::try_from(w).unwrap_or(v1::TrendingWindow::Daily) {
            v1::TrendingWindow::Weekly => 7 * 86_400_000,
            v1::TrendingWindow::Monthly => 30 * 86_400_000,
            _ => 86_400_000,
        }
    }

    /// Changesets landed on `main` within the window, newest first (bounded).
    async fn recent(&self, repo: &RepoCtx, since_ms: u64) -> Vec<bee_history::LogEntry> {
        let history = self.state.history(repo);
        let Ok(tip) = crate::services::query::resolve_rev(&history, None).await else {
            return Vec::new();
        };
        history
            .log(
                &tip,
                &LogOptions {
                    first_parent: true,
                    limit: 500,
                    since: Some((since_ms / 1000) as i64),
                    ..LogOptions::default()
                },
            )
            .await
            .map(|p| p.entries)
            .unwrap_or_default()
    }

    async fn trending_repos(
        &self,
        caller: &Caller,
        r: v1::ListTrendingReposRequest,
    ) -> ApiResult<v1::ListTrendingReposResponse> {
        let now = self.state.now_ms();
        let since = now.saturating_sub(Self::window_ms(r.window));
        let mut ranked: Vec<(u64, u64, v1::TrendingRepo)> = Vec::new();
        for (ctx, f) in self.readable_repos(caller).await? {
            if !r.language.is_empty() && !f.primary_language.eq_ignore_ascii_case(&r.language) {
                continue;
            }
            let entries = self.recent(&ctx, since).await;
            let mut authors: BTreeMap<String, (u64, v1::Principal)> = BTreeMap::new();
            for e in &entries {
                if let Some(m) = &e.meta
                    && !m.creator.is_empty()
                {
                    let ent = authors.entry(m.creator.clone()).or_insert((
                        0,
                        v1::Principal {
                            id: m.creator.clone(),
                            display: m.author.clone(),
                            ..Default::default()
                        },
                    ));
                    ent.0 += 1;
                }
            }
            let mut built: Vec<(u64, v1::Principal)> = authors.into_values().collect();
            built.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
            let activity = entries.len() as u64;
            ranked.push((
                activity,
                f.stars,
                v1::TrendingRepo {
                    repo: Some(repo_hit(&f, Vec::new(), activity as f32)),
                    built_by: built.into_iter().take(5).map(|(_, p)| p).collect(),
                    ..Default::default()
                },
            ));
        }
        ranked.retain(|(a, _, _)| *a > 0);
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(
                a.2.repo
                    .as_ref()
                    .map(|x| x.full_name.clone())
                    .cmp(&b.2.repo.as_ref().map(|x| x.full_name.clone())),
            )
        });
        let all: Vec<v1::TrendingRepo> = ranked
            .into_iter()
            .enumerate()
            .map(|(i, (_, _, mut t))| {
                t.rank = i as u32 + 1;
                t
            })
            .collect();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        Ok(v1::ListTrendingReposResponse {
            repos: page,
            computed_at: Some(timestamp_ms(now)),
            page: next_page(next),
        })
    }

    async fn trending_developers(
        &self,
        caller: &Caller,
        r: v1::ListTrendingDevelopersRequest,
    ) -> ApiResult<v1::ListTrendingDevelopersResponse> {
        let now = self.state.now_ms();
        let since = now.saturating_sub(Self::window_ms(r.window));
        let mut by: BTreeMap<String, DeveloperActivity> = BTreeMap::new();
        for (ctx, f) in self.readable_repos(caller).await? {
            if !r.language.is_empty() && !f.primary_language.eq_ignore_ascii_case(&r.language) {
                continue;
            }
            for e in self.recent(&ctx, since).await {
                let Some(m) = &e.meta else { continue };
                if m.creator.is_empty() {
                    continue;
                }
                let ent =
                    by.entry(m.creator.clone())
                        .or_insert((0, m.author.clone(), BTreeMap::new()));
                ent.0 += 1;
                ent.2.entry(f.full_name.clone()).or_insert((0, f.clone())).0 += 1;
            }
        }
        let mut rows: Vec<(u64, v1::TrendingDeveloper)> = Vec::new();
        for (id, (n, display, repos)) in by {
            let rec = self.state.principal(&id).await.ok().flatten();
            if !r.include_agents && rec.as_ref().is_some_and(|p| p.kind == "agent") {
                continue;
            }
            let popular = repos
                .values()
                .max_by(|a, b| a.0.cmp(&b.0).then(b.1.full_name.cmp(&a.1.full_name)))
                .map(|(_, r)| repo_hit(r, Vec::new(), 0.0));
            let principal = rec
                .as_ref()
                .map(crate::services::auth::principal_to)
                .unwrap_or(v1::Principal {
                    id: id.clone(),
                    display: display.clone(),
                    ..Default::default()
                });
            rows.push((
                n,
                v1::TrendingDeveloper {
                    user: Some(v1::SearchUserHit {
                        login: principal.display.clone(),
                        name: principal.display.clone(),
                        principal: Some(principal),
                        ..Default::default()
                    }),
                    popular_repo: popular,
                    rank: 0,
                },
            ));
        }
        rows.sort_by(|a, b| {
            b.0.cmp(&a.0).then(
                a.1.user
                    .as_ref()
                    .map(|u| u.login.clone())
                    .cmp(&b.1.user.as_ref().map(|u| u.login.clone())),
            )
        });
        let all: Vec<v1::TrendingDeveloper> = rows
            .into_iter()
            .enumerate()
            .map(|(i, (_, mut d))| {
                d.rank = i as u32 + 1;
                d
            })
            .collect();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        Ok(v1::ListTrendingDevelopersResponse {
            developers: page,
            computed_at: Some(timestamp_ms(now)),
            page: next_page(next),
        })
    }

    /// A principal's leaderboard identity: its user hit and whether it is an agent. Service
    /// principals and the server's own writers (no stored record and no ULID, like the
    /// `server` author of an auto-init README) rank on neither board (`None`).
    async fn leader_user(&self, id: &str, display: &str) -> Option<(bool, v1::SearchUserHit)> {
        let rec = self.state.principal(id).await.ok().flatten();
        if rec.is_none() && bee_core::Ulid::parse(id).is_err() {
            return None;
        }
        let kind = rec.as_ref().map(|p| p.kind.as_str()).unwrap_or("human");
        if kind == "service" {
            return None;
        }
        let principal = rec
            .as_ref()
            .map(crate::services::auth::principal_to)
            .unwrap_or(v1::Principal {
                id: id.to_string(),
                display: display.to_string(),
                ..Default::default()
            });
        Some((
            kind == "agent",
            v1::SearchUserHit {
                login: principal.display.clone(),
                name: principal.display.clone(),
                principal: Some(principal),
                ..Default::default()
            },
        ))
    }

    /// Reviews submitted within the window on the change requests of `repo`, by reviewer id
    /// (count, display name).
    async fn reviews_in(
        &self,
        md: &MetadataMap,
        repo: &RepoCtx,
        since_ms: u64,
        by: &mut BTreeMap<String, (u64, String)>,
    ) -> ApiResult<()> {
        use bee_proto::v1::change_service_server::ChangeService;
        let w = self.forge()?;
        let changes =
            bee_forge::changes::service::ChangeServices::new(w.forge.clone(), w.changes.clone());
        let mut token = String::new();
        let mut n = 0usize;
        loop {
            let req = v1::ListChangesRequest {
                repo: repo_ref(&repo.full_name),
                page: Some(v1::PageRequest {
                    page_size: 200,
                    page_token: token.clone(),
                }),
                ..Default::default()
            };
            let Ok(resp) = changes.list_changes(sub(md, req)).await else {
                return Ok(());
            };
            let resp = resp.into_inner();
            for c in resp.changes {
                n += 1;
                let req = v1::ListReviewsRequest {
                    repo: repo_ref(&repo.full_name),
                    change_request_id: c.id.clone(),
                };
                let Ok(reviews) = changes.list_reviews(sub(md, req)).await else {
                    continue;
                };
                for rv in reviews.into_inner().reviews {
                    let Some(who) = rv.reviewer.as_ref().filter(|p| !p.id.is_empty()) else {
                        continue;
                    };
                    if ts_ms(&rv.created_at) < since_ms as i64 {
                        continue;
                    }
                    let ent = by.entry(who.id.clone()).or_insert((0, who.display.clone()));
                    ent.0 += 1;
                }
            }
            token = resp.page.map(|p| p.next_page_token).unwrap_or_default();
            if token.is_empty() || n >= MAX_PER_REPO {
                return Ok(());
            }
        }
    }

    /// Stars `repo` gained within the window.
    async fn stars_in(&self, md: &MetadataMap, repo: &RepoCtx, since_ms: u64) -> ApiResult<u64> {
        use bee_proto::v1::user_service_server::UserService;
        let w = self.forge()?;
        let base = bee_forge::service::ForgeServices::new(w.forge.clone());
        let mut token = String::new();
        let mut n = 0usize;
        let mut gained = 0u64;
        loop {
            let req = v1::ListRepoStargazersRequest {
                repo: repo_ref(&repo.full_name),
                page: Some(v1::PageRequest {
                    page_size: 200,
                    page_token: token.clone(),
                }),
            };
            let Ok(resp) = base.list_repo_stargazers(sub(md, req)).await else {
                return Ok(gained);
            };
            let resp = resp.into_inner();
            for g in &resp.stargazers {
                n += 1;
                if ts_ms(&g.starred_at) >= since_ms as i64 {
                    gained += 1;
                }
            }
            token = resp.page.map(|p| p.next_page_token).unwrap_or_default();
            if token.is_empty() || n >= MAX_PER_REPO {
                return Ok(gained);
            }
        }
    }

    async fn leaderboard(
        &self,
        caller: &Caller,
        md: &MetadataMap,
        r: v1::ListLeaderboardRequest,
    ) -> ApiResult<v1::ListLeaderboardResponse> {
        use v1::{LeaderboardMetric as M, LeaderboardSubject as S};
        let (subject, metric) = leaderboard_kind(r.subject, r.metric)?;
        let now = self.state.now_ms();
        let since = now.saturating_sub(Self::window_ms(r.window));
        let repos = self.readable_repos(caller).await?;
        let mut rows: Vec<LeaderRow> = Vec::new();
        if subject == S::Repos {
            for (ctx, f) in &repos {
                let score = match metric {
                    M::Stars => self.stars_in(md, ctx, since).await?,
                    _ => self.recent(ctx, since).await.len() as u64,
                };
                rows.push(LeaderRow {
                    score,
                    name: f.full_name.clone(),
                    id: f.id.clone(),
                    entry: v1::LeaderboardEntry {
                        repo: Some(repo_hit(f, Vec::new(), score as f32)),
                        ..Default::default()
                    },
                });
            }
        } else {
            // Principal id → (count, display name).
            let mut by: BTreeMap<String, (u64, String)> = BTreeMap::new();
            for (ctx, _) in &repos {
                if metric == M::Reviews {
                    self.reviews_in(md, ctx, since, &mut by).await?;
                    continue;
                }
                for e in self.recent(ctx, since).await {
                    if let Some(m) = &e.meta
                        && !m.creator.is_empty()
                    {
                        by.entry(m.creator.clone())
                            .or_insert((0, m.author.clone()))
                            .0 += 1;
                    }
                }
            }
            for (id, (score, display)) in by {
                let Some((agent, user)) = self.leader_user(&id, &display).await else {
                    continue;
                };
                if agent != (subject == S::Agents) {
                    continue;
                }
                rows.push(LeaderRow {
                    score,
                    name: user.login.clone(),
                    id,
                    entry: v1::LeaderboardEntry {
                        user: Some(user),
                        ..Default::default()
                    },
                });
            }
        }
        let all = rank_leaders(rows);
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        Ok(v1::ListLeaderboardResponse {
            entries: page,
            computed_at: Some(timestamp_ms(now)),
            page: next_page(next),
        })
    }

    async fn collection_proto(
        &self,
        caller: &Caller,
        c: &crate::config::CollectionEntry,
        with_items: bool,
    ) -> v1::TopicCollection {
        let mut items = Vec::new();
        if with_items {
            for it in &c.items {
                use v1::topic_collection_item::Item;
                let item = if it.starts_with("http://") || it.starts_with("https://") {
                    Some(Item::Url(it.clone()))
                } else if let Some(login) = it.strip_prefix('@') {
                    Some(Item::User(v1::SearchUserHit {
                        login: login.to_string(),
                        name: login.to_string(),
                        ..Default::default()
                    }))
                } else {
                    match self.state.repo(repo_ref(it).as_ref()).await {
                        Ok(ctx)
                            if self
                                .state
                                .gate(caller, &ctx, Action::Read, None)
                                .await
                                .is_ok() =>
                        {
                            let f = forge_repo(&self.state, &ctx).await;
                            Some(Item::Repo(repo_hit(&f, Vec::new(), 0.0)))
                        }
                        // Repos the caller cannot read are left out, not hinted at.
                        _ => None,
                    }
                };
                if let Some(item) = item {
                    items.push(v1::TopicCollectionItem {
                        item: Some(item),
                        note: String::new(),
                    });
                }
            }
        }
        v1::TopicCollection {
            slug: c.slug.clone(),
            display_name: c.display_name.clone(),
            description: c.description.clone(),
            image_url: c.image_url.clone(),
            item_count: c.items.len() as u32,
            items,
        }
    }
}

macro_rules! search_rpc {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {
        $crate::unary!($self.state, $req, |$caller, $inner, $meta| $body)
    };
}

#[tonic::async_trait]
impl SearchService for SearchApi {
    type StreamSearchCodeStream = RpcStream<v1::CodeSearchStreamMessage>;

    async fn search_code(
        &self,
        req: tonic::Request<v1::SearchCodeRequest>,
    ) -> Rpc<v1::SearchCodeResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .search_code_impl(&caller, inner))
    }

    async fn stream_search_code(
        &self,
        req: tonic::Request<v1::StreamSearchCodeRequest>,
    ) -> Rpc<Self::StreamSearchCodeStream> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let msgs = self.stream_code_impl(&caller, req.into_inner()).await?;
            Ok::<_, ApiError>(stream_of(msgs.into_iter().map(Ok).collect()))
        }
        .await;
        finish(&meta, r.map(tonic::Response::new))
    }

    async fn search_symbols(
        &self,
        req: tonic::Request<v1::SearchSymbolsRequest>,
    ) -> Rpc<v1::SearchSymbolsResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .search_symbols_impl(&caller, inner))
    }

    async fn get_symbol_definitions(
        &self,
        req: tonic::Request<v1::GetSymbolDefinitionsRequest>,
    ) -> Rpc<v1::GetSymbolDefinitionsResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .definitions_impl(&caller, inner))
    }

    async fn get_symbol_references(
        &self,
        req: tonic::Request<v1::GetSymbolReferencesRequest>,
    ) -> Rpc<v1::GetSymbolReferencesResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .references_impl(&caller, inner))
    }

    async fn get_document_symbols(
        &self,
        req: tonic::Request<v1::GetDocumentSymbolsRequest>,
    ) -> Rpc<v1::GetDocumentSymbolsResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .document_symbols_impl(&caller, inner))
    }

    async fn search_issues(
        &self,
        req: tonic::Request<v1::SearchIssuesRequest>,
    ) -> Rpc<v1::SearchIssuesResponse> {
        let md = req.metadata().clone();
        search_rpc!(self, req, |caller, inner, meta| self
            .search_issues_impl(&caller, &md, inner))
    }

    async fn search_repos(
        &self,
        req: tonic::Request<v1::SearchReposRequest>,
    ) -> Rpc<v1::SearchReposResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .search_repos_impl(&caller, inner))
    }

    async fn search_changesets(
        &self,
        req: tonic::Request<v1::SearchChangesetsRequest>,
    ) -> Rpc<v1::SearchChangesetsResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .search_changesets_impl(&caller, inner))
    }

    async fn search_users(
        &self,
        req: tonic::Request<v1::SearchUsersRequest>,
    ) -> Rpc<v1::SearchUsersResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .search_users_impl(&caller, inner))
    }

    async fn search_topics(
        &self,
        req: tonic::Request<v1::SearchTopicsRequest>,
    ) -> Rpc<v1::SearchTopicsResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .search_topics_impl(&caller, inner))
    }

    async fn search_discussions(
        &self,
        req: tonic::Request<v1::SearchDiscussionsRequest>,
    ) -> Rpc<v1::SearchDiscussionsResponse> {
        let md = req.metadata().clone();
        search_rpc!(self, req, |caller, inner, meta| self
            .search_discussions_impl(&caller, &md, inner))
    }

    async fn search_packages(
        &self,
        req: tonic::Request<v1::SearchPackagesRequest>,
    ) -> Rpc<v1::SearchPackagesResponse> {
        let md = req.metadata().clone();
        search_rpc!(self, req, |caller, inner, meta| self
            .search_packages_impl(&caller, &md, inner))
    }

    async fn parse_search_query(
        &self,
        req: tonic::Request<v1::ParseSearchQueryRequest>,
    ) -> Rpc<v1::ParseSearchQueryResponse> {
        search_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            let kind = query_kind(inner.kind)?;
            let out = bee_search::parse_search_query(kind, &inner.query);
            let me = me_login(&caller);
            Ok::<_, ApiError>(v1::ParseSearchQueryResponse {
                valid: out.valid,
                qualifiers: out
                    .qualifiers
                    .iter()
                    .map(|q| v1::SearchQualifier {
                        key: q.key.clone(),
                        value: q.value.clone(),
                        negated: q.negated,
                        regex: q.regex,
                        range: Some(v1::SearchRange {
                            start: q.span.start,
                            end: q.span.end,
                        }),
                        resolved: if q.resolved == "@me" {
                            me.clone()
                        } else {
                            q.resolved.clone()
                        },
                    })
                    .collect(),
                errors: out
                    .errors
                    .iter()
                    .map(|e| v1::SearchQueryError {
                        message: e.message.clone(),
                        subcode: e.subcode.to_string(),
                        range: Some(v1::SearchRange {
                            start: e.span.start,
                            end: e.span.end,
                        }),
                    })
                    .collect(),
                normalized: out.normalized,
            })
        })
    }

    async fn create_saved_search(
        &self,
        req: tonic::Request<v1::CreateSavedSearchRequest>,
    ) -> Rpc<v1::SavedSearch> {
        search_rpc!(self, req, |caller, inner, meta| self
            .create_saved(&caller, inner, &meta))
    }

    async fn get_saved_search(
        &self,
        req: tonic::Request<v1::GetSavedSearchRequest>,
    ) -> Rpc<v1::SavedSearch> {
        search_rpc!(self, req, |caller, inner, meta| async {
            Ok::<_, ApiError>(saved_proto(
                &self.saved(&caller, &inner.saved_search_id).await?,
            ))
        })
    }

    async fn list_saved_searches(
        &self,
        req: tonic::Request<v1::ListSavedSearchesRequest>,
    ) -> Rpc<v1::ListSavedSearchesResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .list_saved(&caller, inner))
    }

    async fn update_saved_search(
        &self,
        req: tonic::Request<v1::UpdateSavedSearchRequest>,
    ) -> Rpc<v1::SavedSearch> {
        search_rpc!(self, req, |caller, inner, meta| self
            .update_saved(&caller, inner, &meta))
    }

    async fn delete_saved_search(
        &self,
        req: tonic::Request<v1::DeleteSavedSearchRequest>,
    ) -> Rpc<v1::DeleteSavedSearchResponse> {
        search_rpc!(self, req, |caller, inner, meta| async {
            self.delete_saved(&caller, &inner.saved_search_id, &meta)
                .await?;
            Ok::<_, ApiError>(v1::DeleteSavedSearchResponse {})
        })
    }

    async fn list_trending_repos(
        &self,
        req: tonic::Request<v1::ListTrendingReposRequest>,
    ) -> Rpc<v1::ListTrendingReposResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .trending_repos(&caller, inner))
    }

    async fn list_trending_developers(
        &self,
        req: tonic::Request<v1::ListTrendingDevelopersRequest>,
    ) -> Rpc<v1::ListTrendingDevelopersResponse> {
        search_rpc!(self, req, |caller, inner, meta| self
            .trending_developers(&caller, inner))
    }

    async fn list_leaderboard(
        &self,
        req: tonic::Request<v1::ListLeaderboardRequest>,
    ) -> Rpc<v1::ListLeaderboardResponse> {
        let md = req.metadata().clone();
        search_rpc!(self, req, |caller, inner, meta| self
            .leaderboard(&caller, &md, inner))
    }

    async fn get_topic(&self, req: tonic::Request<v1::GetTopicRequest>) -> Rpc<v1::Topic> {
        search_rpc!(self, req, |caller, inner, meta| async {
            let name = inner.name.trim().to_lowercase();
            if name.is_empty() {
                return Err(ApiError::invalid("missing_field", "name is required"));
            }
            let topics = self.topics(&caller).await?;
            let top = if inner.top_repo_count == 0 {
                10
            } else {
                inner.top_repo_count.min(100) as usize
            };
            match topics.get(&name) {
                Some(repos) => Ok(self.topic_proto(&name, repos, top)),
                None if self
                    .state
                    .cfg
                    .search
                    .featured_topics
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(&name)) =>
                {
                    Ok(self.topic_proto(&name, &[], top))
                }
                None => Err(ApiError::not_found(format!("topic {name}"))),
            }
        })
    }

    async fn list_featured_topics(
        &self,
        req: tonic::Request<v1::ListFeaturedTopicsRequest>,
    ) -> Rpc<v1::ListFeaturedTopicsResponse> {
        search_rpc!(self, req, |caller, inner, meta| async {
            let topics = self.topics(&caller).await?;
            let mut out: Vec<v1::Topic> = self
                .state
                .cfg
                .search
                .featured_topics
                .iter()
                .map(|t| {
                    let n = t.to_lowercase();
                    let repos = topics.get(&n).cloned().unwrap_or_default();
                    self.topic_proto(&n, &repos, 0)
                })
                .collect();
            out.dedup_by(|a, b| a.name == b.name);
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&out, &token, size)?;
            Ok::<_, ApiError>(v1::ListFeaturedTopicsResponse {
                topics: page,
                page: next_page(next),
            })
        })
    }

    async fn list_topic_collections(
        &self,
        req: tonic::Request<v1::ListTopicCollectionsRequest>,
    ) -> Rpc<v1::ListTopicCollectionsResponse> {
        search_rpc!(self, req, |caller, inner, meta| async {
            let mut out = Vec::new();
            for c in &self.state.cfg.search.collections {
                out.push(self.collection_proto(&caller, c, false).await);
            }
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&out, &token, size)?;
            Ok::<_, ApiError>(v1::ListTopicCollectionsResponse {
                collections: page,
                page: next_page(next),
            })
        })
    }

    async fn get_topic_collection(
        &self,
        req: tonic::Request<v1::GetTopicCollectionRequest>,
    ) -> Rpc<v1::TopicCollection> {
        search_rpc!(self, req, |caller, inner, meta| async {
            let c = self
                .state
                .cfg
                .search
                .collections
                .iter()
                .find(|c| c.slug == inner.slug)
                .ok_or_else(|| ApiError::not_found(format!("collection {}", inner.slug)))?;
            Ok::<_, ApiError>(self.collection_proto(&caller, c, true).await)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_scope_terms() {
        assert_eq!(
            word_at("fn charge_card(x)", 1, 5).as_deref(),
            Some("charge_card")
        );
        assert_eq!(word_at("a b", 1, 1), None);
        let (scope, rest) =
            split_scope(ForgeKind::Issues, "is:open repo:dev/x -org:y bug").unwrap();
        assert_eq!(scope.len(), 2);
        assert_eq!(rest, "is:open bug");
        assert_eq!(strip_type_terms("is:pr is:open type:issue x"), "is:open x");
        let f = Filter::And(vec![
            Filter::Pred(Predicate::Flag {
                field: "archived",
                value: false,
            }),
            Filter::Not(Box::new(Filter::Pred(Predicate::Flag {
                field: "x",
                value: true,
            }))),
        ]);
        assert!(eval(&f, &|p| matches!(
            p,
            Predicate::Flag {
                field: "archived",
                ..
            }
        )));
        assert!(in_range(5, &Bound::Included(5), &Bound::Unbounded));
        assert!(!in_range(5, &Bound::Excluded(5), &Bound::Unbounded));
    }

    fn row(score: u64, name: &str, id: &str) -> LeaderRow {
        LeaderRow {
            score,
            name: name.into(),
            id: id.into(),
            entry: v1::LeaderboardEntry {
                user: Some(v1::SearchUserHit {
                    login: name.into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    fn order(rows: Vec<LeaderRow>) -> Vec<(u32, u64, String)> {
        rank_leaders(rows)
            .into_iter()
            .map(|e| (e.rank, e.score, e.user.unwrap_or_default().login))
            .collect()
    }

    #[test]
    fn leaderboards_rank_by_score_then_name() {
        let got = order(vec![
            row(2, "carol", "3"),
            row(5, "bob", "2"),
            row(2, "alice", "1"),
            row(0, "dave", "4"),
            row(7, "erin", "5"),
        ]);
        assert_eq!(
            got,
            vec![
                (1, 7, "erin".into()),
                (2, 5, "bob".into()),
                (3, 2, "alice".into()),
                (4, 2, "carol".into()),
            ],
            "ties order by name, and a zero score is not ranked"
        );
    }

    #[test]
    fn leaderboard_ties_on_name_fall_back_to_id_whatever_the_input_order() {
        let a = order(vec![row(3, "same", "B"), row(3, "same", "A")]);
        let b = order(vec![row(3, "same", "A"), row(3, "same", "B")]);
        assert_eq!(a, b);
        let ids: Vec<String> = rank_leaders(vec![row(3, "same", "B"), row(3, "same", "A")])
            .into_iter()
            .map(|e| e.rank.to_string())
            .collect();
        assert_eq!(ids, vec!["1", "2"]);
        // Reversing the input never changes the result.
        let fwd: Vec<LeaderRow> = (0..20)
            .map(|i| row(i % 4, &format!("u{}", i % 3), &i.to_string()))
            .collect();
        let rev: Vec<LeaderRow> = (0..20)
            .rev()
            .map(|i| row(i % 4, &format!("u{}", i % 3), &i.to_string()))
            .collect();
        assert_eq!(order(fwd), order(rev));
    }

    #[test]
    fn leaderboard_subjects_and_metrics_must_fit() {
        use v1::{LeaderboardMetric as M, LeaderboardSubject as S};
        assert_eq!(leaderboard_kind(0, 0).unwrap(), (S::Agents, M::Lands));
        assert_eq!(
            leaderboard_kind(S::Humans as i32, M::Reviews as i32).unwrap(),
            (S::Humans, M::Reviews)
        );
        assert_eq!(
            leaderboard_kind(S::Repos as i32, M::Stars as i32).unwrap(),
            (S::Repos, M::Stars)
        );
        assert_eq!(
            leaderboard_kind(S::Repos as i32, 0).unwrap(),
            (S::Repos, M::Lands)
        );
        for (s, m) in [
            (S::Repos as i32, M::Reviews as i32),
            (S::Agents as i32, M::Stars as i32),
            (S::Humans as i32, M::Stars as i32),
            (0, M::Stars as i32),
            (99, 0),
            (0, 99),
        ] {
            let e = leaderboard_kind(s, m).unwrap_err();
            assert_eq!(e.detail.reason, "INVALID_ARGUMENT", "{s} {m}");
            assert_eq!(e.detail.subcode, "invalid_field");
        }
    }
}
