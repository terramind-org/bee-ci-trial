//! `QueryService`: history without a clone (SPEC §8, §9.3). Every answer is filtered by the
//! caller's read access: unreadable delta entries are dropped (`filtered = true`), restricted
//! tree entries are listed but not descended into, and reads of restricted paths are refused.

use std::sync::Arc;

use bee_core::{ObjectId, ObjectKind, RefName, RepoPath};
use bee_history::{
    BlameOptions, DiffOptions, FileChange, History, LineKind, LogOptions, PathContent,
};
use bee_object::{Changeset, DeltaEntry, Meta};
use bee_policy::{Action, Policy};
use bee_proto::v1::{self, ErrorReason, query_service_server::QueryService};
use bee_store::{MinVersion, RepoId};

use crate::auth::Caller;
use crate::convert::{self, changeset_to, id_from, id_to, mode_to};
use crate::errors::{ApiError, ApiResult};
use crate::services::object::finish;
use crate::services::{Rpc, RpcStream, stream_of};
use crate::state::{AppState, RepoCtx, RequestMeta};

/// Default and maximum page sizes.
const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 1000;
/// Default `max_files` for Diff.
const DEFAULT_MAX_FILES: usize = 3000;

/// The query service.
pub struct Queries {
    state: Arc<AppState>,
}

impl Queries {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> Queries {
        Queries { state }
    }
}

/// Page size from a request.
pub fn page_size(p: Option<&v1::PageRequest>) -> usize {
    match p.map(|p| p.page_size as usize) {
        Some(0) | None => DEFAULT_PAGE,
        Some(n) => n.min(MAX_PAGE),
    }
}

/// Page token from a request.
pub fn page_token(p: Option<&v1::PageRequest>) -> Option<String> {
    p.map(|p| p.page_token.clone()).filter(|t| !t.is_empty())
}

/// Resolves a revision (default `main`); ChangeIds are looked up on `main`.
pub async fn resolve_rev(history: &History, rev: Option<&v1::Revision>) -> ApiResult<ObjectId> {
    let expr = rev
        .map(|r| r.expr.as_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("main");
    let main = RefName::new("main").map_err(ApiError::from)?;
    history
        .resolve_str(expr, Some(&main))
        .await
        .map_err(ApiError::from)
}

/// The ref a revision expression starts from, if it names one (for `min_version`).
fn rev_ref(rev: Option<&v1::Revision>) -> Option<RefName> {
    let expr = rev
        .map(|r| r.expr.as_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("main");
    match bee_core::Revision::parse(expr).ok()?.base {
        bee_core::RevisionBase::Ref(n) | bee_core::RevisionBase::RefAt(n, _) => Some(n),
        _ => None,
    }
}

/// The readable view of a changeset's delta and whether anything was dropped.
pub fn filter_delta(
    state: &AppState,
    caller: &Caller,
    repo: RepoId,
    policy: &Policy,
    cs: &Changeset,
) -> (Vec<DeltaEntry>, bool) {
    let actors = state.known_policy_actors(caller, repo);
    let allowed = state.path_filter(caller, &actors, policy, Action::Read);
    let kept: Vec<DeltaEntry> = cs
        .delta()
        .iter()
        .filter(|e| allowed(&e.path.to_string()))
        .cloned()
        .collect();
    let filtered = kept.len() != cs.delta().len();
    (kept, filtered)
}

/// A changeset as the caller may see it.
pub async fn view(
    state: &AppState,
    caller: &Caller,
    policy: &Policy,
    history: &History,
    id: &ObjectId,
    include_meta: bool,
) -> ApiResult<v1::Changeset> {
    let cs = history.changeset(id).await?;
    let (delta, filtered) = filter_delta(state, caller, history.repo(), policy, &cs);
    // A changeset whose every change is restricted does not reveal its meta.
    let hide_meta = filtered && delta.is_empty();
    let meta: Option<Meta> = if include_meta && !hide_meta {
        history.meta_object(cs.meta()).await?
    } else {
        None
    };
    let generation = history
        .graph_node(id)
        .await
        .map(|n| n.generation)
        .unwrap_or(0);
    Ok(changeset_to(
        id,
        &cs,
        &delta,
        filtered,
        meta.as_ref(),
        generation,
    ))
}

fn require_read(
    state: &AppState,
    caller: &Caller,
    repo: &RepoCtx,
    policy: &Policy,
    path: &str,
) -> ApiResult<()> {
    if state.path_allowed(caller, repo.repo.id, policy, Action::Read, path) {
        Ok(())
    } else {
        Err(ApiError::policy(
            ErrorReason::PolicyDenied,
            "access",
            vec![format!("access:read:{path}")],
            format!("read access to /{path} is denied"),
        ))
    }
}

/// Maps a read failure on quarantined (redacted) content to `REDACTED`.
async fn redacted_or(state: &AppState, repo: &RepoCtx, e: bee_core::BeeError) -> ApiError {
    if e.reason == bee_core::Reason::MissingObject
        && let Some(bee_core::PayloadValue::Str(t)) = e.payload.get("object")
        && let Ok(id) = ObjectId::parse(t)
        && crate::redaction::is_redacted(state, repo.org.id, &id)
            .await
            .unwrap_or(false)
    {
        return ApiError::new(ErrorReason::Redacted, format!("{id} is redacted"));
    }
    e.into()
}

fn status_of(e: &ApiError) -> i32 {
    match e.reason() {
        ErrorReason::NotFound | ErrorReason::MissingObject => v1::ItemStatus::NotFound as i32,
        ErrorReason::PolicyDenied | ErrorReason::PermissionDenied => {
            v1::ItemStatus::PermissionDenied as i32
        }
        ErrorReason::Redacted => v1::ItemStatus::Redacted as i32,
        _ => v1::ItemStatus::NotFound as i32,
    }
}

impl Queries {
    async fn ctx(
        &self,
        caller: &Caller,
        repo: Option<&v1::RepoRef>,
    ) -> ApiResult<(RepoCtx, Arc<Policy>)> {
        let repo = self.state.repo(repo).await?;
        let policy = self.state.gate(caller, &repo, Action::Read, None).await?;
        Ok((repo, policy))
    }

    async fn get_changesets_impl(
        &self,
        caller: &Caller,
        req: v1::GetChangesetsRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::GetChangesetsResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let history = s.history(&repo);
        let mut resp = v1::GetChangesetsResponse::default();
        for raw in &req.ids {
            let id = id_from(Some(raw), "ids")?;
            if id.kind() != ObjectKind::Changeset {
                return Err(ApiError::invalid(
                    "wrong_kind",
                    format!("{id} is not a changeset id"),
                ));
            }
            match view(s, caller, &policy, &history, &id, req.include_meta).await {
                Ok(cs) => {
                    resp.changesets.push(cs.clone());
                    resp.results.push(v1::ChangesetResult {
                        id: Some(id_to(&id)),
                        status: v1::ItemStatus::Ok as i32,
                        changeset: Some(cs),
                    });
                }
                Err(e) => resp.results.push(v1::ChangesetResult {
                    id: Some(id_to(&id)),
                    status: status_of(&e),
                    changeset: None,
                }),
            }
        }
        Ok((repo, policy, resp))
    }

    async fn read_path_impl(
        &self,
        caller: &Caller,
        req: v1::ReadPathRequest,
    ) -> ApiResult<(
        RepoCtx,
        Arc<Policy>,
        Vec<Result<v1::ReadPathChunk, tonic::Status>>,
    )> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let history = s.history(&repo);
        let at = resolve_rev(&history, req.at.as_ref()).await?;
        let path = convert::path_from(&req.path)?;
        require_read(s, caller, &repo, &policy, &path.to_string())?;
        let content = match history.read_path(&at, Some(&path)).await {
            Ok(c) => c,
            Err(e) => return Err(redacted_or(s, &repo, e).await),
        };
        let chunks = match content {
            PathContent::File { item, content } => {
                if content.is_redacted()
                    || crate::redaction::is_redacted(s, repo.org.id, &item.id).await?
                {
                    return Err(ApiError::new(
                        ErrorReason::Redacted,
                        format!("{path} is redacted"),
                    ));
                }
                let bytes = content.bytes_or_empty();
                let mut out = Vec::new();
                let mut pos = 0;
                loop {
                    let next = (pos + super::object::CHUNK).min(bytes.len());
                    out.push(Ok(v1::ReadPathChunk {
                        mode: mode_to(item.mode),
                        blob: Some(id_to(&item.id)),
                        size: item.size,
                        data: bytes[pos..next].to_vec(),
                        last: next >= bytes.len(),
                    }));
                    pos = next;
                    if pos >= bytes.len() {
                        break;
                    }
                }
                out
            }
            PathContent::Gitlink { item } => vec![Ok(v1::ReadPathChunk {
                mode: mode_to(item.mode),
                blob: Some(id_to(&item.id)),
                size: 0,
                data: Vec::new(),
                last: true,
            })],
            PathContent::Dir { .. } => {
                return Err(ApiError::invalid(
                    "is_directory",
                    format!("{path} is a directory; use ListTree"),
                ));
            }
        };
        Ok((repo, policy, chunks))
    }

    async fn list_tree_impl(
        &self,
        caller: &Caller,
        req: v1::ListTreeRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::ListTreeResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let history = s.history(&repo);
        let at = resolve_rev(&history, req.at.as_ref()).await?;
        let path = convert::opt_path_from(&req.path)?;
        if let Some(p) = &path {
            require_read(s, caller, &repo, &policy, &p.to_string())?;
        }
        let page = history
            .list_tree(
                &at,
                path.as_ref(),
                req.recursive,
                page_size(req.page.as_ref()),
                page_token(req.page.as_ref()).as_deref(),
            )
            .await?;
        let mut restricted_dirs: Vec<RepoPath> = Vec::new();
        let mut entries = Vec::new();
        for item in page.items {
            if restricted_dirs.iter().any(|d| d.is_ancestor_of(&item.path)) {
                continue;
            }
            let restricted = !s.path_allowed(
                caller,
                repo.repo.id,
                &policy,
                Action::Read,
                &item.path.to_string(),
            );
            if restricted && item.mode == bee_core::Mode::Tree {
                restricted_dirs.push(item.path.clone());
            }
            entries.push(v1::TreeEntry {
                path: item.path.as_bytes().to_vec(),
                mode: mode_to(item.mode),
                id: Some(id_to(&item.id)),
                size: item.size,
                restricted,
            });
        }
        Ok((
            repo,
            policy,
            v1::ListTreeResponse {
                entries,
                page: Some(v1::PageResponse {
                    next_page_token: page.next_page_token.unwrap_or_default(),
                }),
            },
        ))
    }

    async fn log_impl(
        &self,
        caller: &Caller,
        req: v1::LogRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::LogResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let min = (req.min_version > 0).then(|| match rev_ref(req.from.as_ref()) {
            Some(name) => MinVersion::Ref {
                repo: repo.repo.id,
                name,
                version: req.min_version,
            },
            None => MinVersion::Event {
                repo: repo.repo.id,
                seq: req.min_version,
            },
        });
        let history = s.history_min(&repo, min);
        let from = resolve_rev(&history, req.from.as_ref()).await?;
        let path = convert::opt_path_from(&req.path)?;
        if let Some(p) = &path {
            require_read(s, caller, &repo, &policy, &p.to_string())?;
        }
        let opts = LogOptions {
            first_parent: req.first_parent,
            limit: page_size(req.page.as_ref()),
            page_token: page_token(req.page.as_ref()),
            since: req.since.as_ref().map(|t| t.seconds),
            until: req.until.as_ref().map(|t| t.seconds),
            changed_paths: false,
        };
        let author = req.author.trim().to_string();
        let keep = |m: Option<&Meta>| {
            author.is_empty()
                || m.is_some_and(|m| m.creator == author || m.author.contains(author.as_str()))
        };
        let mut out = v1::LogResponse::default();
        let next = if path.is_some() {
            let page = history.path_log(&from, path.as_ref(), &opts).await?;
            for e in &page.entries {
                if keep(e.meta.as_ref()) {
                    out.changesets.push(
                        view(s, caller, &policy, &history, &e.changeset, req.include_meta).await?,
                    );
                }
            }
            page.next_page_token
        } else {
            let page = history.log(&from, &opts).await?;
            for e in &page.entries {
                if keep(e.meta.as_ref()) {
                    out.changesets
                        .push(view(s, caller, &policy, &history, &e.id, req.include_meta).await?);
                }
            }
            page.next_page_token
        };
        out.page = Some(v1::PageResponse {
            next_page_token: next.unwrap_or_default(),
        });
        Ok((repo, policy, out))
    }

    async fn blame_impl(
        &self,
        caller: &Caller,
        req: v1::BlameRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::BlameResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let history = s.history(&repo);
        let at = resolve_rev(&history, req.at.as_ref()).await?;
        let path = convert::path_from(&req.path)?;
        require_read(s, caller, &repo, &policy, &path.to_string())?;
        let mut ignore = Vec::new();
        for raw in &req.ignore_revs {
            ignore.push(id_from(Some(raw), "ignore_revs")?);
        }
        if req.use_ignore_revs_file {
            ignore.extend(history.blame_ignore_revs(&at).await?);
        }
        let result = match history
            .blame(
                &at,
                &path,
                &BlameOptions {
                    ignore_revs: ignore,
                },
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(redacted_or(s, &repo, e).await),
        };
        let total = result.line_count() as u32;
        let lo = req.start_line.max(1);
        let hi = if req.end_line == 0 {
            total
        } else {
            req.end_line.min(total)
        };
        let mut ranges = Vec::new();
        for r in &result.ranges {
            let start = r.start as u32 + 1;
            let end = start + r.len as u32 - 1;
            let (a, b) = (start.max(lo), end.min(hi));
            if a > b {
                continue;
            }
            ranges.push(v1::BlameRange {
                start_line: a,
                line_count: b - a + 1,
                changeset: Some(id_to(&r.changeset)),
                original_start_line: r.origin_start as u32 + 1 + (a - start),
            });
        }
        Ok((repo, policy, v1::BlameResponse { ranges }))
    }

    async fn diff_impl(
        &self,
        caller: &Caller,
        req: v1::DiffRequest,
    ) -> ApiResult<(
        RepoCtx,
        Arc<Policy>,
        Vec<Result<v1::DiffItem, tonic::Status>>,
    )> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let history = s.history(&repo);
        let to = resolve_rev(&history, req.to.as_ref()).await?;
        let from = match req.from.as_ref().filter(|r| !r.expr.is_empty()) {
            Some(r) => Some(resolve_rev(&history, Some(r)).await?),
            None => history.changeset(&to).await?.first_parent().copied(),
        };
        let mut paths = Vec::new();
        for p in &req.paths {
            if let Some(p) = convert::opt_path_from(p)? {
                paths.push(p);
            }
        }
        let opts = DiffOptions {
            paths,
            hunks: req.include_hunks,
            context: if req.context_lines == 0 {
                3
            } else {
                req.context_lines as usize
            },
            detect_renames: req.detect_renames || req.detect_copies,
            ignore_whitespace: req.ignore_whitespace,
        };
        let files = match history.diff(from.as_ref(), &to, &opts).await {
            Ok(f) => f,
            Err(e) => return Err(redacted_or(s, &repo, e).await),
        };
        let classes =
            if req.classify && from == history.changeset(&to).await?.first_parent().copied() {
                s.derived(&repo).diff_class(&to).await.ok().map(|v| {
                    v.per_path
                        .into_iter()
                        .map(|(p, c, _)| (p, c.as_str().to_string()))
                        .collect::<std::collections::BTreeMap<_, _>>()
                })
            } else {
                None
            };
        let max = if req.max_files == 0 {
            DEFAULT_MAX_FILES
        } else {
            req.max_files as usize
        };
        let readable: Vec<_> = files
            .into_iter()
            .filter(|f| {
                s.path_allowed(
                    caller,
                    repo.repo.id,
                    &policy,
                    Action::Read,
                    &f.path.to_string(),
                ) && match &f.change {
                    FileChange::Renamed { from, .. } => s.path_allowed(
                        caller,
                        repo.repo.id,
                        &policy,
                        Action::Read,
                        &from.to_string(),
                    ),
                    _ => true,
                }
            })
            .collect();
        let truncated = readable.len() > max;
        let n = readable.len().min(max);
        let mut items = Vec::with_capacity(n);
        for (i, f) in readable.into_iter().take(max).enumerate() {
            let side = |e: &Option<bee_history::pathhist::LeafEntry>| {
                e.as_ref().map(|e| v1::ConflictSide {
                    mode: mode_to(e.mode),
                    blob: Some(id_to(&e.id)),
                })
            };
            let (status, old_path, similarity) = match &f.change {
                FileChange::Added => (v1::DiffStatus::Added, Vec::new(), 0),
                FileChange::Deleted => (v1::DiffStatus::Deleted, Vec::new(), 0),
                FileChange::Modified => {
                    let mode_only = matches!((&f.old, &f.new), (Some(o), Some(n)) if o.id == n.id && o.mode != n.mode);
                    (
                        if mode_only {
                            v1::DiffStatus::ModeChanged
                        } else {
                            v1::DiffStatus::Modified
                        },
                        Vec::new(),
                        0,
                    )
                }
                FileChange::Renamed { from, similarity } => (
                    v1::DiffStatus::Renamed,
                    from.as_bytes().to_vec(),
                    match similarity {
                        None => 100,
                        Some((shared, union)) if *union > 0 => (shared * 100 / union) as u32,
                        Some(_) => 0,
                    },
                ),
            };
            let hunks = f
                .hunks
                .iter()
                .map(|h| v1::DiffHunk {
                    old_start: h.old_start as u32 + 1,
                    old_len: h.old_len as u32,
                    new_start: h.new_start as u32 + 1,
                    new_len: h.new_len as u32,
                    lines: h
                        .lines
                        .iter()
                        .map(|l| {
                            let prefix = match l.kind {
                                LineKind::Context => ' ',
                                LineKind::Added => '+',
                                LineKind::Removed => '-',
                            };
                            format!("{prefix}{}", String::from_utf8_lossy(&l.content))
                        })
                        .collect(),
                })
                .collect();
            items.push(Ok(v1::DiffItem {
                diff_class: classes
                    .as_ref()
                    .and_then(|c| c.get(&f.path).cloned())
                    .unwrap_or_default(),
                path: f.path.as_bytes().to_vec(),
                old: side(&f.old),
                new: side(&f.new),
                binary: f.binary,
                hunks,
                status: status as i32,
                old_path,
                similarity,
                additions: f.added as u32,
                deletions: f.removed as u32,
                truncated: truncated && i + 1 == n,
            }));
        }
        Ok((repo, policy, items))
    }

    async fn get_derived_impl(
        &self,
        caller: &Caller,
        req: v1::GetDerivedRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::GetDerivedResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let derived = s.derived(&repo);
        let t =
            s.registry.get(&req.r#type).cloned().ok_or_else(|| {
                ApiError::not_found(format!("unknown derived type {:?}", req.r#type))
            })?;
        if t.content_bearing() {
            require_read(s, caller, &repo, &policy, "")?;
        }
        let mut key = id_from(req.key.as_ref(), "key")?;
        // Per-path types (keyed by a file history node) also take a changeset and a path.
        if t.key_kind() == bee_derived::KeyKind::HistoryNode
            && !req.path.is_empty()
            && key.kind() == ObjectKind::Changeset
        {
            let path = RepoPath::new(req.path.clone()).map_err(ApiError::from)?;
            require_read(s, caller, &repo, &policy, &path.to_string())?;
            key = s
                .history(&repo)
                .node_at(&key, Some(&path))
                .await?
                .map(|(node, _)| node)
                .ok_or_else(|| ApiError::not_found(format!("{path} not found at {key}")))?;
        }
        let version = if req.version == 0 {
            t.version()
        } else {
            req.version
        };
        let (value, derived_now) = if version != t.version() || !req.derive_if_missing {
            match derived.read_stored(&req.r#type, version, &key).await? {
                Some(v) => (v, false),
                None => {
                    return Err(ApiError::not_found(
                        "no stored value; set derive_if_missing to derive it",
                    ));
                }
            }
        } else {
            let v = derived.get(&req.r#type, &key).await?;
            (v.value, v.derived)
        };
        let value_json = if req.r#type == "diff_class" {
            bee_derived::DiffClassValue::decode(&value)
                .map(|v| {
                    serde_json::json!({
                        "highest": v.highest.map(|c| c.as_str()),
                        "classifier": v.classifier,
                        "per_path": v.per_path.iter().map(|(p, c, rule)| serde_json::json!({
                            "path": p.to_string(), "class": c.as_str(), "rule": rule,
                        })).collect::<Vec<_>>(),
                    })
                    .to_string()
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok((
            repo,
            policy,
            v1::GetDerivedResponse {
                r#type: req.r#type.clone(),
                version,
                key: Some(id_to(&key)),
                value,
                value_json,
                derived_now,
                content_bearing: t.content_bearing(),
            },
        ))
    }

    async fn compare_impl(
        &self,
        caller: &Caller,
        req: v1::CompareRevisionsRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::CompareRevisionsResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        let history = s.history(&repo);
        let base = resolve_rev(&history, req.base.as_ref()).await?;
        let head = resolve_rev(&history, req.head.as_ref()).await?;
        let bases = history.merge_bases(&base, &head).await?;
        let max = if req.max_changesets == 0 {
            250
        } else {
            req.max_changesets as usize
        };
        // Walk head's history until it reaches the merge bases' ancestry.
        let mut ahead = Vec::new();
        let mut truncated = false;
        let mut token = None;
        'outer: loop {
            let page = history
                .log(
                    &head,
                    &LogOptions {
                        limit: 200,
                        page_token: token.clone(),
                        ..LogOptions::default()
                    },
                )
                .await?;
            for e in &page.entries {
                let mut contained = false;
                for b in &bases {
                    if history.is_ancestor(&e.id, b).await? {
                        contained = true;
                        break;
                    }
                }
                if contained || history.is_ancestor(&e.id, &base).await? {
                    continue;
                }
                if ahead.len() >= max {
                    truncated = true;
                    break 'outer;
                }
                ahead.push(e.id);
            }
            match page.next_page_token {
                Some(t) if ahead.len() < max * 4 => token = Some(t),
                _ => break,
            }
        }
        let mut behind = 0u64;
        let mut token = None;
        loop {
            let page = history
                .log(
                    &base,
                    &LogOptions {
                        limit: 200,
                        page_token: token.clone(),
                        ..LogOptions::default()
                    },
                )
                .await?;
            let mut stop = true;
            for e in &page.entries {
                if !history.is_ancestor(&e.id, &head).await? {
                    behind += 1;
                    stop = false;
                }
            }
            match page.next_page_token {
                Some(t) if !stop && behind < 100_000 => token = Some(t),
                _ => break,
            }
        }
        let mut changesets = Vec::new();
        for id in &ahead {
            changesets.push(view(s, caller, &policy, &history, id, true).await?);
        }
        let mut files = Vec::new();
        if req.include_files {
            let from = bases.first().copied();
            for f in history
                .diff(from.as_ref(), &head, &DiffOptions::default())
                .await?
            {
                if !s.path_allowed(
                    caller,
                    repo.repo.id,
                    &policy,
                    Action::Read,
                    &f.path.to_string(),
                ) {
                    continue;
                }
                files.push(v1::DiffItem {
                    path: f.path.as_bytes().to_vec(),
                    status: match f.change {
                        FileChange::Added => v1::DiffStatus::Added,
                        FileChange::Deleted => v1::DiffStatus::Deleted,
                        FileChange::Modified => v1::DiffStatus::Modified,
                        FileChange::Renamed { .. } => v1::DiffStatus::Renamed,
                    } as i32,
                    ..Default::default()
                });
            }
        }
        Ok((
            repo,
            policy,
            v1::CompareRevisionsResponse {
                merge_bases: bases.iter().map(id_to).collect(),
                ahead_by: ahead.len() as u64,
                behind_by: behind,
                changesets,
                files,
                truncated,
            },
        ))
    }

    async fn find_copies_impl(
        &self,
        caller: &Caller,
        req: v1::FindCopiesRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::FindCopiesResponse)> {
        let s = &self.state;
        let (repo, policy) = self.ctx(caller, req.repo.as_ref()).await?;
        if req.min_similarity != 0 && req.min_similarity < 100 {
            return Err(ApiError::invalid(
                "unsupported_similarity",
                "only exact copies (min_similarity 100) are indexed",
            ));
        }
        if req.across_org {
            return Err(ApiError::invalid(
                "unsupported_scope",
                "across_org copy search needs the search service",
            ));
        }
        let history = s.history(&repo);
        let at = resolve_rev(&history, req.at.as_ref()).await?;
        let path = convert::path_from(&req.path)?;
        require_read(s, caller, &repo, &policy, &path.to_string())?;
        let root = *history.changeset(&at).await?.root();
        let entry = history
            .entry_at(&root, &path)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("{path} not found")))?;
        let mut copies = Vec::new();
        for p in s
            .meta
            .object_paths(repo.repo.id, &entry.id, bee_store::ReadFrom::Primary)
            .await?
        {
            let Some(p) = p else { continue };
            if p == path
                || !s.path_allowed(caller, repo.repo.id, &policy, Action::Read, &p.to_string())
            {
                continue;
            }
            copies.push(v1::CopyLocation {
                repo_id: repo.id_text(),
                path: p.as_bytes().to_vec(),
                blob: Some(id_to(&entry.id)),
                similarity: 100,
                first_seen: None,
            });
        }
        let size = page_size(req.page.as_ref());
        copies.truncate(size);
        Ok((
            repo,
            policy,
            v1::FindCopiesResponse {
                copies,
                page: Some(v1::PageResponse::default()),
            },
        ))
    }
}

macro_rules! unary {
    ($self:ident, $req:ident, $impl:ident) => {{
        let meta = RequestMeta::of(&$req);
        let r = async {
            let caller = $self.state.caller($req.metadata()).await?;
            let (repo, policy, resp) = $self.$impl(&caller, $req.into_inner()).await?;
            $self
                .state
                .with_epochs(&repo, &policy, tonic::Response::new(resp))
                .await
        }
        .await;
        finish(&meta, r)
    }};
}

macro_rules! streaming {
    ($self:ident, $req:ident, $impl:ident) => {{
        let meta = RequestMeta::of(&$req);
        let r = async {
            let caller = $self.state.caller($req.metadata()).await?;
            let (repo, policy, items) = $self.$impl(&caller, $req.into_inner()).await?;
            $self
                .state
                .with_epochs(&repo, &policy, tonic::Response::new(stream_of(items)))
                .await
        }
        .await;
        finish(&meta, r)
    }};
}

pub(crate) use unary;

#[tonic::async_trait]
impl QueryService for Queries {
    async fn get_changesets(
        &self,
        req: tonic::Request<v1::GetChangesetsRequest>,
    ) -> Rpc<v1::GetChangesetsResponse> {
        unary!(self, req, get_changesets_impl)
    }

    type ReadPathStream = RpcStream<v1::ReadPathChunk>;

    async fn read_path(
        &self,
        req: tonic::Request<v1::ReadPathRequest>,
    ) -> Rpc<Self::ReadPathStream> {
        streaming!(self, req, read_path_impl)
    }

    async fn list_tree(
        &self,
        req: tonic::Request<v1::ListTreeRequest>,
    ) -> Rpc<v1::ListTreeResponse> {
        unary!(self, req, list_tree_impl)
    }

    async fn log(&self, req: tonic::Request<v1::LogRequest>) -> Rpc<v1::LogResponse> {
        unary!(self, req, log_impl)
    }

    async fn blame(&self, req: tonic::Request<v1::BlameRequest>) -> Rpc<v1::BlameResponse> {
        unary!(self, req, blame_impl)
    }

    type DiffStream = RpcStream<v1::DiffItem>;

    async fn diff(&self, req: tonic::Request<v1::DiffRequest>) -> Rpc<Self::DiffStream> {
        streaming!(self, req, diff_impl)
    }

    async fn is_ancestor(
        &self,
        req: tonic::Request<v1::IsAncestorRequest>,
    ) -> Rpc<v1::IsAncestorResponse> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let req = req.into_inner();
            let (repo, policy) = self.ctx(&caller, req.repo.as_ref()).await?;
            let h = self.state.history(&repo);
            let a = resolve_rev(&h, req.ancestor.as_ref()).await?;
            let d = resolve_rev(&h, req.descendant.as_ref()).await?;
            let is_ancestor = h.is_ancestor(&a, &d).await?;
            self.state
                .with_epochs(
                    &repo,
                    &policy,
                    tonic::Response::new(v1::IsAncestorResponse { is_ancestor }),
                )
                .await
        }
        .await;
        finish(&meta, r)
    }

    async fn merge_base(
        &self,
        req: tonic::Request<v1::MergeBaseRequest>,
    ) -> Rpc<v1::MergeBaseResponse> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let req = req.into_inner();
            let (repo, policy) = self.ctx(&caller, req.repo.as_ref()).await?;
            let h = self.state.history(&repo);
            let a = resolve_rev(&h, req.a.as_ref()).await?;
            let b = resolve_rev(&h, req.b.as_ref()).await?;
            let bases = h.merge_bases(&a, &b).await?.iter().map(id_to).collect();
            self.state
                .with_epochs(
                    &repo,
                    &policy,
                    tonic::Response::new(v1::MergeBaseResponse { bases }),
                )
                .await
        }
        .await;
        finish(&meta, r)
    }

    async fn resolve(&self, req: tonic::Request<v1::ResolveRequest>) -> Rpc<v1::ResolveResponse> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let req = req.into_inner();
            let (repo, policy) = self.ctx(&caller, req.repo.as_ref()).await?;
            let h = self.state.history(&repo);
            let mut resp = v1::ResolveResponse::default();
            for rev in &req.revisions {
                match resolve_rev(&h, Some(rev)).await {
                    Ok(id) => {
                        resp.ids.push(id_to(&id));
                        resp.statuses.push(v1::ItemStatus::Ok as i32);
                    }
                    Err(e)
                        if matches!(
                            e.reason(),
                            ErrorReason::InvalidArgument | ErrorReason::AmbiguousPrefix
                        ) =>
                    {
                        return Err(e);
                    }
                    Err(e) => {
                        resp.ids.push(v1::ObjectId::default());
                        resp.statuses.push(status_of(&e));
                    }
                }
            }
            self.state
                .with_epochs(&repo, &policy, tonic::Response::new(resp))
                .await
        }
        .await;
        finish(&meta, r)
    }

    async fn get_derived(
        &self,
        req: tonic::Request<v1::GetDerivedRequest>,
    ) -> Rpc<v1::GetDerivedResponse> {
        unary!(self, req, get_derived_impl)
    }

    async fn list_derived_types(
        &self,
        req: tonic::Request<v1::ListDerivedTypesRequest>,
    ) -> Rpc<v1::ListDerivedTypesResponse> {
        let meta = RequestMeta::of(&req);
        let r = async {
            self.state.caller(req.metadata()).await?;
            let types = self
                .state
                .registry
                .types()
                .map(|t| v1::DerivedType {
                    name: t.name().to_string(),
                    version: t.version(),
                    mode: t.mode().as_str().to_string(),
                    key_kind: match t.key_kind() {
                        bee_derived::KeyKind::Changeset => "changeset",
                        bee_derived::KeyKind::HistoryNode => "history_node",
                    }
                    .into(),
                    content_bearing: t.content_bearing(),
                    description: String::new(),
                })
                .collect();
            Ok(tonic::Response::new(v1::ListDerivedTypesResponse { types }))
        }
        .await;
        finish(&meta, r)
    }

    async fn compare_revisions(
        &self,
        req: tonic::Request<v1::CompareRevisionsRequest>,
    ) -> Rpc<v1::CompareRevisionsResponse> {
        unary!(self, req, compare_impl)
    }

    async fn find_copies(
        &self,
        req: tonic::Request<v1::FindCopiesRequest>,
    ) -> Rpc<v1::FindCopiesResponse> {
        unary!(self, req, find_copies_impl)
    }
}
