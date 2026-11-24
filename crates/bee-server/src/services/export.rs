//! Exports (`ImportService.StartExport`, `GetExport`, `ListExports`, `CancelExport`;
//! `docs/spec/operations.md` §7), in the formats the importer reads back:
//!
//! - **`GITHUB_ARCHIVE`**: a gzipped tar laid out like a GitHub org migration archive, the
//!   format `bee_github::archive` imports. Each repo is a bare git repository under
//!   `repositories/<org>/<repo>.git/` (refs, `HEAD`, one pack with its index) whose commits
//!   carry the exact git ids the git gateway serves (imported history keeps its original ids);
//!   with `include_metadata`, numbered JSON arrays (`repositories_*`, `users_*`, `labels` in
//!   the repository record, `milestones_*`, `issues_*`, `pull_requests_*`,
//!   `pull_request_reviews_*`, `issue_comments_*`, `releases_*`) refer to each other by URL
//!   under the server's public URL, as GitHub's do. The archive is stored as an object of the
//!   org (an upload by the requester) and served by signed URL.
//! - **`GIT_PUSH`**: the same bare repository pushed to `remote_url` with `git push --mirror`
//!   (the system `git`, as imports use it). HTTP(S) remotes get `remote_token` as a Basic
//!   credential header; local paths must lie under `import.allowed_roots`.
//!
//! Exports run in the background and never block writes; `CancelExport` stops one between
//! repos. Starting one needs `manage_settings` in the org and read access to every path of
//! each repo (git carries whole trees).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bee_core::{ObjectId, RefName};
use bee_git_http::advertise;
use bee_git_http::gitmap::GitView;
use bee_git_http::walk::{FetchArgs, plan};
use bee_git_http::{GitError, GitResult, PushContext, RefState, RepoSession};
use bee_github::archive::{TarEntry, write_tar};
use bee_history::History;
use bee_object::Changeset;
use bee_policy::{Action, Policy};
use bee_proto::v1::{self, ErrorReason};
use bee_store::{OrgId, OrgRow, UploadRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::kv::KvOp;
use crate::services::support::{get_json, next_page, offset_page, page_of, put_json, scan_json};
use crate::state::{AppState, RepoCtx, RequestMeta};
use crate::util::timestamp_ms;

/// Export rows (`<id>`).
pub const NS_EXPORT: &str = "export";

/// A stored export.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ExportRec {
    /// Id.
    pub id: String,
    /// Org id.
    pub org_id: String,
    /// Org name.
    pub org: String,
    /// Repo ids.
    pub repo_ids: Vec<String>,
    /// `ExportFormat`.
    pub format: i32,
    /// `ExportState`.
    pub state: i32,
    /// Per-category progress.
    pub progress: Vec<(String, u64, u64)>,
    /// The archive object (blob id text).
    pub blob: String,
    /// Remote of a push (without credentials).
    pub remote: String,
    /// Include issues, CRs, reviews and releases.
    pub include_metadata: bool,
    /// Who started it.
    pub principal: String,
    /// Failure.
    pub error: String,
    /// Started.
    pub started_ms: u64,
    /// Finished.
    pub finished_ms: u64,
}

fn proto(state: &AppState, r: &ExportRec, url: Option<(String, u64)>) -> v1::Export {
    let _ = state;
    v1::Export {
        id: r.id.clone(),
        org: r.org.clone(),
        repo_ids: r.repo_ids.clone(),
        format: r.format,
        state: r.state,
        progress: r
            .progress
            .iter()
            .map(|(c, d, t)| v1::ImportProgress {
                category: c.clone(),
                done: *d,
                total: *t,
            })
            .collect(),
        download_url: url.as_ref().map(|u| u.0.clone()).unwrap_or_default(),
        download_url_expires_unix: url.map(|u| (u.1 / 1000) as i64).unwrap_or(0),
        started_at: Some(timestamp_ms(r.started_ms)),
        finished_at: (r.finished_ms > 0).then(|| timestamp_ms(r.finished_ms)),
        error: r.error.clone(),
    }
}

/// Whether the caller may still read every repo an export carries (the archive URL is minted
/// afresh on every read, so a repo made private, deleted, or out of the caller's reach since
/// the export ran withholds it).
async fn still_readable(state: &AppState, caller: &Caller, r: &ExportRec) -> bool {
    for id in &r.repo_ids {
        let rr = v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(id.clone())),
        };
        let Ok(ctx) = state.repo(Some(&rr)).await else {
            return false;
        };
        if state
            .authorize_repo(caller, &ctx, Action::Read)
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

async fn with_url(state: &AppState, caller: &Caller, r: &ExportRec) -> ApiResult<v1::Export> {
    if r.state != v1::ExportState::Succeeded as i32 || r.blob.is_empty() {
        return Ok(proto(state, r, None));
    }
    if !still_readable(state, caller, r).await {
        let mut p = proto(state, r, None);
        p.error =
            "the archive is withheld: the caller can no longer read every exported repo".into();
        return Ok(p);
    }
    if state.url_issuer.is_none() {
        let mut p = proto(state, r, None);
        p.error = "signed URLs are disabled on this server (blobs.signed_urls)".into();
        return Ok(p);
    }
    let org = OrgId(bee_core::Ulid::parse(&r.org_id).map_err(ApiError::from)?);
    let id = ObjectId::parse(&r.blob).map_err(ApiError::from)?;
    let ttl = Duration::from_secs(state.cfg.blobs.signed_url_ttl_secs);
    let mut ctxs = Vec::new();
    for rid in &r.repo_ids {
        let rr = v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(rid.clone())),
        };
        ctxs.push(state.repo(Some(&rr)).await?);
    }
    let refs: Vec<&RepoCtx> = ctxs.iter().collect();
    let u = state.issue_url(org, &id, ttl, Some(caller), &refs).await?;
    Ok(proto(state, r, Some((u.url, u.expires_at_ms))))
}

/// Requires `manage_settings` in the org under its default policy.
async fn org_manager(state: &AppState, caller: &Caller, org: &OrgRow) -> ApiResult<()> {
    let text = state.org_text(org).await?;
    let policy = Policy::from_sources(Some(&text), None)
        .map_err(|e| ApiError::new(ErrorReason::Internal, format!("invalid org policy: {e}")))?;
    for a in &caller.actors {
        if !policy.actions_of(a).contains(&Action::ManageSettings) {
            return Err(ApiError::policy(
                ErrorReason::PolicyDenied,
                "members",
                vec!["role:manage_settings".into()],
                "exports need manage_settings in the org",
            ));
        }
    }
    Ok(())
}

/// Whether the caller reads every path of the repo (the root and each access-rule prefix).
fn reads_everything(state: &AppState, caller: &Caller, repo: &RepoCtx, policy: &Policy) -> bool {
    std::iter::once("")
        .chain(policy.doc().access.keys().map(String::as_str))
        .all(|p| state.path_allowed(caller, repo.repo.id, policy, Action::Read, p))
}

/// The repos an org owns: its VCS repos the forge does not give to another org (forks of
/// other orgs living in this namespace, repos transferred away) or record as deleted, plus
/// repos transferred in from another namespace.
async fn org_repos(state: &AppState, org: &OrgRow) -> ApiResult<Vec<RepoCtx>> {
    let org_id = org.id.0.to_text();
    let by_id = |id: String| v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::Id(id)),
    };
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for r in state.meta.list_repos(org.id).await? {
        let ctx = state.repo(Some(&by_id(r.id.0.to_text()))).await?;
        if let Some(rec) = state.repo_record(&ctx).await?
            && rec.id == ctx.id_text()
            && (rec.org_id != org_id
                || crate::access::lifecycle(&rec) == crate::access::Lifecycle::Deleted)
        {
            continue;
        }
        seen.insert(ctx.id_text());
        out.push(ctx);
    }
    if let Some(w) = state.hub.get() {
        let cx = crate::forge::landing::quick_cx();
        let mut after = String::new();
        loop {
            let (page, next) = w
                .forge
                .orgs
                .list_repos(&cx, &org_id, &after, 500)
                .await
                .map_err(crate::forge::forge_to_api)?;
            for rec in page {
                if seen.insert(rec.id.clone())
                    && let Ok(ctx) = state.repo(Some(&by_id(rec.id.clone()))).await
                {
                    out.push(ctx);
                }
            }
            if next.is_empty() {
                break;
            }
            after = next;
        }
    }
    Ok(out)
}

/// `StartExport`.
pub async fn start(
    state: &Arc<AppState>,
    caller: &Caller,
    req: v1::StartExportRequest,
    meta: &RequestMeta,
) -> ApiResult<v1::Export> {
    let org = crate::services::support::org_by_name_or_id(state, &req.org).await?;
    org_manager(state, caller, &org).await?;
    let format = match v1::ExportFormat::try_from(req.format) {
        Ok(v1::ExportFormat::Unspecified) | Ok(v1::ExportFormat::GithubArchive) => {
            v1::ExportFormat::GithubArchive
        }
        Ok(v1::ExportFormat::GitPush) => v1::ExportFormat::GitPush,
        Err(_) => return Err(ApiError::invalid("bad_format", "unknown export format")),
    };
    let mut repos: Vec<RepoCtx> = Vec::new();
    if req.repos.is_empty() {
        repos = org_repos(state, &org).await?;
    } else {
        for r in &req.repos {
            let ctx = state.repo(Some(r)).await?;
            if state.policy_org(&ctx).await?.id != org.id {
                return Err(ApiError::invalid(
                    "wrong_org",
                    format!("{} is not in org {}", ctx.full_name, org.name),
                ));
            }
            repos.push(ctx);
        }
    }
    for r in &repos {
        let policy = state.gate(caller, r, Action::Read, None).await?;
        if !reads_everything(state, caller, r, &policy) {
            return Err(ApiError::policy(
                ErrorReason::PolicyDenied,
                "access",
                vec![format!("access:read:{}", r.full_name)],
                "exports carry whole trees: the caller may not read every path",
            ));
        }
    }
    let remote = if format == v1::ExportFormat::GitPush {
        if repos.len() != 1 {
            return Err(ApiError::invalid(
                "one_repo",
                "a git push export names exactly one repo",
            ));
        }
        validate_remote(state, &req.remote_url)?;
        req.remote_url.clone()
    } else {
        String::new()
    };
    let now = state.now_ms();
    let rec = ExportRec {
        id: crate::util::new_ulid(now)?.to_text(),
        org_id: org.id.0.to_text(),
        org: org.name.clone(),
        repo_ids: repos.iter().map(RepoCtx::id_text).collect(),
        format: format as i32,
        state: v1::ExportState::Queued as i32,
        remote: remote.clone(),
        include_metadata: req.include_metadata,
        principal: caller.id().to_string(),
        started_ms: now,
        ..ExportRec::default()
    };
    put_json(state.kv.as_ref(), NS_EXPORT, &rec.id, &rec).await?;
    state
        .audit(
            caller,
            meta,
            None,
            &rec.id,
            "export.start",
            json!({ "org": org.name, "repos": rec.repo_ids, "format": format.as_str_name(), "remote": remote }),
        )
        .await?;
    let s = state.clone();
    let job = rec.clone();
    let token = req.remote_token.clone();
    tokio::spawn(async move {
        let fin = match run(&s, job.clone(), repos, &token).await {
            Ok(r) => r,
            Err(e) => ExportRec {
                state: v1::ExportState::Failed as i32,
                error: e.detail.message.clone(),
                finished_ms: s.now_ms(),
                ..job
            },
        };
        // A cancellation that landed meanwhile wins: the final write is a compare-and-set
        // against the record as read, so a concurrent CancelExport is never overwritten.
        let id = fin.id.clone();
        let _ = update_rec(&s, &id, |cur| {
            (cur.state != v1::ExportState::Cancelled as i32).then(|| fin.clone())
        })
        .await;
    });
    Ok(proto(state, &rec, None))
}

fn validate_remote(state: &AppState, url: &str) -> ApiResult<()> {
    if url.is_empty() {
        return Err(ApiError::invalid("missing_field", "remote_url is required"));
    }
    if url.starts_with("https://") || url.starts_with("http://") || url.starts_with("ssh://") {
        return Ok(());
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    let p = std::path::Path::new(path);
    if !p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(ApiError::invalid(
            "bad_remote",
            "remote_url is an http(s) or ssh URL, or an absolute local path",
        ));
    }
    if !state
        .cfg
        .import
        .allowed_roots
        .iter()
        .any(|r| !r.is_empty() && p.starts_with(r))
    {
        return Err(ApiError::invalid(
            "bad_remote",
            "local-path remotes are limited to import.allowed_roots",
        ));
    }
    Ok(())
}

/// Updates an export record with a compare-and-set against the bytes read, retrying on a
/// concurrent write. `f` returns the new record, or `None` to leave it unchanged; the result is
/// the record written, or `None` when `f` declined.
async fn update_rec(
    state: &AppState,
    id: &str,
    f: impl Fn(&ExportRec) -> Option<ExportRec>,
) -> ApiResult<Option<ExportRec>> {
    let kv = state.kv.as_ref();
    for _ in 0..100 {
        let cur = kv.get(NS_EXPORT, id.as_bytes()).await?;
        let rec: ExportRec = cur
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .ok_or_else(|| ApiError::not_found("export not found"))?;
        let Some(next) = f(&rec) else {
            return Ok(None);
        };
        let bytes = serde_json::to_vec(&next)
            .map_err(|e| ApiError::new(ErrorReason::Internal, e.to_string()))?;
        let ops = vec![
            KvOp::expect(NS_EXPORT, id.as_bytes().to_vec(), cur),
            KvOp::put(NS_EXPORT, id.as_bytes().to_vec(), bytes),
        ];
        if kv.apply(ops).await? {
            return Ok(Some(next));
        }
    }
    Err(ApiError::new(
        ErrorReason::Unavailable,
        "export record contended",
    ))
}

/// Writes the job's progress with a compare-and-set, refusing with `CANCELLED` once a
/// cancellation has landed, so no intermediate write can resurrect a cancelled export.
async fn save_unless_cancelled(state: &AppState, rec: &ExportRec) -> ApiResult<()> {
    let written = update_rec(state, &rec.id, |cur| {
        (cur.state != v1::ExportState::Cancelled as i32).then(|| rec.clone())
    })
    .await?;
    match written {
        Some(_) => Ok(()),
        None => Err(ApiError::new(ErrorReason::Cancelled, "export cancelled")),
    }
}

async fn cancelled(state: &AppState, id: &str) -> bool {
    matches!(
        get_json::<ExportRec>(state.kv.as_ref(), NS_EXPORT, id).await,
        Ok(Some(r)) if r.state == v1::ExportState::Cancelled as i32
    )
}

async fn run(
    state: &Arc<AppState>,
    mut rec: ExportRec,
    repos: Vec<RepoCtx>,
    token: &str,
) -> ApiResult<ExportRec> {
    // A cancellation that landed while the export was queued must not be overwritten.
    rec.state = v1::ExportState::Running as i32;
    save_unless_cancelled(state, &rec).await?;
    let mut entries: Vec<TarEntry> = Vec::new();
    let mut meta_files: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let total = repos.len() as u64;
    for (i, repo) in repos.iter().enumerate() {
        if cancelled(state, &rec.id).await {
            return Err(ApiError::new(ErrorReason::Cancelled, "export cancelled"));
        }
        let bare = bare_repo(state, repo).await?;
        if rec.format == v1::ExportFormat::GitPush as i32 {
            push(state, &bare, &rec.remote, token).await?;
        } else {
            let prefix = format!("repositories/{}.git", repo.full_name);
            for (path, data) in &bare.files {
                entries.push(TarEntry {
                    path: format!("{prefix}/{path}"),
                    dir: false,
                    data: data.clone(),
                });
            }
            if rec.include_metadata {
                metadata(state, repo, &bare, &mut meta_files).await?;
            }
        }
        rec.progress = vec![("repositories".into(), i as u64 + 1, total)];
        // Progress must not overwrite a cancellation that landed during this repo.
        save_unless_cancelled(state, &rec).await?;
    }
    if rec.format == v1::ExportFormat::GithubArchive as i32 {
        for (kind, items) in &meta_files {
            rec.progress
                .push((kind.clone(), items.len() as u64, items.len() as u64));
            entries.push(TarEntry {
                path: format!("{kind}_000001.json"),
                dir: false,
                data: serde_json::to_vec_pretty(items).unwrap_or_default(),
            });
        }
        let tar = write_tar(&entries);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &tar)
            .map_err(|e| ApiError::new(ErrorReason::Internal, format!("gzip: {e}")))?;
        let bytes = gz
            .finish()
            .map_err(|e| ApiError::new(ErrorReason::Internal, format!("gzip: {e}")))?;
        let id = bee_object::blob_id(&bytes);
        let org = OrgId(bee_core::Ulid::parse(&rec.org_id).map_err(ApiError::from)?);
        state
            .writer
            .upload(
                org,
                vec![(id, bytes)],
                Some(UploadRecord {
                    repo: None,
                    principal: rec.principal.clone(),
                }),
                state.now_ms(),
            )
            .await?;
        rec.blob = id.to_text();
    }
    rec.state = v1::ExportState::Succeeded as i32;
    rec.finished_ms = state.now_ms();
    Ok(rec)
}

/// A repo as a bare git repository: files relative to the `.git` directory.
pub struct BareRepo {
    /// `(path, bytes)`.
    pub files: Vec<(String, Vec<u8>)>,
    /// Git ref name → commit id (hex), branches and tags.
    pub refs: BTreeMap<String, String>,
    /// Changeset id → commit id (hex) of every ref target.
    pub commits: BTreeMap<String, String>,
}

/// A read-only session for exports: sees every ref; refuses writes.
struct ExportSession {
    history: History,
    full_name: String,
    max_blob: u64,
}

fn read_only() -> GitError {
    GitError::named("PERMISSION_DENIED", "export sessions are read-only")
}

#[async_trait]
impl RepoSession for ExportSession {
    fn history(&self) -> &History {
        &self.history
    }
    fn principal(&self) -> &str {
        ""
    }
    fn principal_label(&self) -> String {
        "export".into()
    }
    fn full_name(&self) -> &str {
        &self.full_name
    }
    fn path_writable(&self, _path: &str) -> bool {
        false
    }
    async fn may_force_unlock(&self) -> GitResult<bool> {
        Ok(false)
    }
    fn ref_visible(&self, _name: &RefName) -> bool {
        true
    }
    fn max_blob_bytes(&self) -> u64 {
        self.max_blob
    }
    async fn check_commit(&self, _c: &Changeset, _p: &PushContext) -> GitResult<()> {
        Err(read_only())
    }
    async fn committed(&self, _c: &ObjectId) -> GitResult<()> {
        Err(read_only())
    }
    async fn is_protected(&self, _n: &RefName) -> GitResult<bool> {
        Ok(true)
    }
    async fn land(&self, _n: &RefName, _h: &ObjectId) -> GitResult<RefState> {
        Err(read_only())
    }
    async fn write_ref(
        &self,
        _n: &RefName,
        _v: u64,
        _t: Option<ObjectId>,
        _f: bool,
    ) -> GitResult<RefState> {
        Err(read_only())
    }
}

fn git_err(e: GitError) -> ApiError {
    let reason = bee_proto::errors::parse_reason(&e.reason).unwrap_or(ErrorReason::Internal);
    ApiError::new(reason, e.message)
}

/// Builds the bare repository of `repo`: branches and tags (not workspaces), one pack.
pub async fn bare_repo(state: &AppState, repo: &RepoCtx) -> ApiResult<BareRepo> {
    let session = ExportSession {
        history: state.history(repo),
        full_name: repo.full_name.clone(),
        max_blob: state.cfg.limits.max_blob_bytes,
    };
    let format = advertise::repo_format(&session).await.map_err(git_err)?;
    let mut view = GitView::new(&session.history, format);
    let list = advertise::list(&session, &mut view)
        .await
        .map_err(git_err)?;
    let mut refs = BTreeMap::new();
    let mut commits = BTreeMap::new();
    let mut wants = Vec::new();
    for r in &list.refs {
        if !(r.name.starts_with("refs/heads/") || r.name.starts_with("refs/tags/"))
            || r.bee.as_str().starts_with("ws/")
        {
            continue;
        }
        refs.insert(r.name.clone(), r.oid.to_hex());
        commits.insert(r.target.to_text(), r.peeled.unwrap_or(r.oid).to_hex());
        if !wants.contains(&r.oid) {
            wants.push(r.oid);
        }
    }
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let head = if refs.contains_key(&list.head) {
        list.head.clone()
    } else {
        refs.keys()
            .find(|n| n.starts_with("refs/heads/"))
            .cloned()
            .unwrap_or_else(|| "refs/heads/main".into())
    };
    files.push(("HEAD".into(), format!("ref: {head}\n").into_bytes()));
    let fmt_line = if format == bee_git::ObjectFormat::Sha256 {
        "\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n"
    } else {
        "\trepositoryformatversion = 0\n"
    };
    files.push((
        "config".into(),
        format!("[core]\n{fmt_line}\tfilemode = true\n\tbare = true\n").into_bytes(),
    ));
    files.push((
        "description".into(),
        format!("{} exported from Beehive\n", repo.full_name).into_bytes(),
    ));
    if !wants.is_empty() {
        let args = FetchArgs {
            wants: wants.clone(),
            haves: Vec::new(),
            shallow: Vec::new(),
            deepen: None,
            filter: None,
            ofs_delta: true,
            include_tag: true,
            thin_pack: false,
        };
        let p = plan(&mut view, &args, &list.tags).await.map_err(git_err)?;
        let pack = bee_git::Pack::parse(&p.pack, format).map_err(ApiError::from)?;
        let idx = pack.index(&|_| Ok(None)).map_err(ApiError::from)?;
        let name = format!("pack-{}", pack.checksum().to_hex());
        files.push((
            format!("objects/pack/{name}.idx"),
            idx.write().map_err(ApiError::from)?,
        ));
        files.push((format!("objects/pack/{name}.pack"), p.pack));
    }
    files.push(("objects/info/packs".into(), Vec::new()));
    for (name, oid) in &refs {
        files.push((name.clone(), format!("{oid}\n").into_bytes()));
    }
    Ok(BareRepo {
        files,
        refs,
        commits,
    })
}

/// Pushes a bare repository to `remote` with `git push --mirror` (`token` as HTTP Basic
/// `x-access-token`).
pub(crate) async fn push(
    state: &AppState,
    bare: &BareRepo,
    remote: &str,
    token: &str,
) -> ApiResult<()> {
    let dir = state
        .cfg
        .server
        .data_dir
        .join("exports")
        .join(crate::util::random_hex(8));
    for (p, data) in &bare.files {
        crate::backup::sql::write_durable(&dir.join(p), data)?;
    }
    std::fs::create_dir_all(dir.join("refs/tags"))
        .map_err(|e| ApiError::new(ErrorReason::Internal, format!("export dir: {e}")))?;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir").arg(&dir);
    if !token.is_empty() && (remote.starts_with("https://") || remote.starts_with("http://")) {
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: Basic {basic}"));
    }
    cmd.args(["push", "--mirror", remote]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = tokio::time::timeout(Duration::from_secs(3600), cmd.output())
        .await
        .map_err(|_| ApiError::new(ErrorReason::DeadlineExceeded, "git push timed out"))?
        .map_err(|e| ApiError::new(ErrorReason::Unavailable, format!("running git: {e}")))?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).replace(token, "<redacted>");
        return Err(ApiError::new(
            ErrorReason::Unavailable,
            format!("git push failed: {}", err.trim()),
        ));
    }
    Ok(())
}

/// A stable numeric id for a ULID-keyed record (GitHub archives key records by number).
fn num_id(id: &str) -> u64 {
    let h = blake3::hash(format!("swarm-export-id\0{id}").as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&h.as_bytes()[..8]);
    (u64::from_be_bytes(b) >> 12).max(1)
}

fn iso(t: &Option<pbjson_types::Timestamp>) -> Value {
    match t {
        Some(t) => {
            let secs = t.seconds;
            let days = secs.div_euclid(86_400);
            let rem = secs.rem_euclid(86_400);
            let (y, m, d) = civil(days);
            Value::String(format!(
                "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
                rem / 3600,
                (rem % 3600) / 60,
                rem % 60
            ))
        }
        None => Value::Null,
    }
}

/// Days since 1970-01-01 to a civil date (proleptic Gregorian).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

async fn login_of(w: &crate::forge::ForgeWiring, p: &Option<v1::Principal>) -> Option<String> {
    let p = p.as_ref()?;
    let (logins, _) = w.forge.hooks.directory.identities(&p.id).await;
    Some(
        logins
            .into_iter()
            .next()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| {
                if p.display.is_empty() {
                    p.id.clone()
                } else {
                    p.display.clone()
                }
            }),
    )
}

/// Adds the repo's forge records to the archive's JSON files.
async fn metadata(
    state: &AppState,
    repo: &RepoCtx,
    bare: &BareRepo,
    files: &mut BTreeMap<String, Vec<Value>>,
) -> ApiResult<()> {
    let Some(w) = state.hub.get() else {
        return Ok(());
    };
    let fe = crate::forge::forge_to_api;
    let cx = bee_forge::Cx::system().with_timeout(Duration::from_secs(600));
    let base = state.public_url();
    let repo_url = format!("{base}/{}", repo.full_name);
    let user_url = |l: &str| format!("{base}/{l}");
    let frepo = w
        .forge
        .orgs
        .repo_any(&cx, &repo.id_text())
        .await
        .map_err(fe)?;
    let info = bee_forge::work::RepoInfo::from(&frepo);
    let mut users: BTreeMap<String, Value> = BTreeMap::new();
    let user = |l: Option<String>, users: &mut BTreeMap<String, Value>| -> Value {
        match l {
            Some(l) => {
                users.entry(l.clone()).or_insert_with(
                    || json!({ "type": "user", "login": l, "name": l, "url": user_url(&l) }),
                );
                Value::String(user_url(&l))
            }
            None => Value::Null,
        }
    };
    // Labels and milestones.
    let (labels, _) = w
        .forge
        .work
        .list_labels(&cx, &info, "", 10_000, "")
        .await
        .map_err(fe)?;
    files.entry("repositories".into()).or_default().push(json!({
        "type": "repository",
        "url": repo_url,
        "name": repo.repo.name,
        "description": frepo.description,
        "default_branch": frepo.default_ref,
        "labels": labels.iter().map(|l| json!({
            "url": format!("{repo_url}/labels/{}", l.name.replace(' ', "%20")),
            "name": l.name, "color": l.color, "description": l.description,
        })).collect::<Vec<_>>(),
    }));
    let (milestones, _) = w
        .forge
        .work
        .list_milestones(&cx, &info, None, 10_000, "")
        .await
        .map_err(fe)?;
    let ms_url = |n: u64| format!("{repo_url}/milestone/{n}");
    for m in &milestones {
        files.entry("milestones".into()).or_default().push(json!({
            "type": "milestone",
            "url": ms_url(m.number),
            "repository": repo_url,
            "title": m.title,
            "description": m.description,
            "state": if m.state == v1::MilestoneState::Closed as i32 { "closed" } else { "open" },
            "due_on": iso(&m.due_on),
            "created_at": iso(&m.created_at),
            "closed_at": iso(&m.closed_at),
        }));
    }
    let label_list = |ls: &[v1::Label]| -> Vec<Value> {
        ls.iter()
            .map(|l| Value::String(format!("{repo_url}/labels/{}", l.name.replace(' ', "%20"))))
            .collect()
    };
    // Issues and their comments.
    let q = bee_forge::work::IssueListQuery {
        state: bee_forge::work::StateSelector::All,
        ..Default::default()
    };
    let mut token = String::new();
    loop {
        let page = w
            .forge
            .work
            .list_issues(&cx, &info, &q, 200, &token)
            .await
            .map_err(fe)?;
        for i in &page.issues {
            let url = format!("{repo_url}/issues/{}", i.number);
            let mut assignees = Vec::new();
            for a in &i.assignees {
                assignees.push(user(login_of(w, &Some(a.clone())).await, &mut users));
            }
            let author = user(login_of(w, &i.author).await, &mut users);
            files.entry("issues".into()).or_default().push(json!({
                "type": "issue",
                "url": url,
                "repository": repo_url,
                "user": author,
                "title": i.title,
                "body": i.body,
                "state": if i.state == v1::IssueState::Closed as i32 { "closed" } else { "open" },
                "state_reason": match v1::IssueStateReason::try_from(i.state_reason) {
                    Ok(v1::IssueStateReason::Completed) => Value::from("completed"),
                    Ok(v1::IssueStateReason::NotPlanned) => Value::from("not_planned"),
                    Ok(v1::IssueStateReason::Duplicate) => Value::from("duplicate"),
                    _ => Value::Null,
                },
                "labels": label_list(&i.labels),
                "assignees": assignees,
                "milestone": i.milestone.as_ref().map(|m| Value::String(ms_url(m.number))).unwrap_or(Value::Null),
                "locked": i.locked,
                "created_at": iso(&i.created_at),
                "updated_at": iso(&i.updated_at),
                "closed_at": iso(&i.closed_at),
            }));
            let mut ct = String::new();
            loop {
                let (comments, next) = w
                    .forge
                    .work
                    .list_comments(&cx, &info, i.number, None, 500, &ct)
                    .await
                    .map_err(fe)?;
                for c in &comments {
                    let u = user(login_of(w, &c.author).await, &mut users);
                    files
                        .entry("issue_comments".into())
                        .or_default()
                        .push(json!({
                            "type": "issue_comment",
                            "url": format!("{url}#issuecomment-{}", num_id(&c.id)),
                            "issue": url,
                            "user": u,
                            "body": c.body,
                            "created_at": iso(&c.created_at),
                            "updated_at": iso(&c.updated_at),
                        }));
                }
                if next.is_empty() {
                    break;
                }
                ct = next;
            }
        }
        if page.next.is_empty() {
            break;
        }
        token = page.next;
    }
    // Change requests, conversation comments and reviews.
    if let Some(changes) = w.forge.changes() {
        let f = bee_forge::changes::query::ListFilter::default();
        let mut token = String::new();
        loop {
            let (crs, next) = changes
                .list(&cx, &frepo, &f, &token, 200)
                .await
                .map_err(fe)?;
            for cr in &crs {
                let url = format!("{repo_url}/pull/{}", cr.number);
                let rev = cr.revisions.last();
                let sha_of = |id: Option<&v1::ObjectId>| -> Value {
                    id.and_then(|o| bee_core::ObjectId::from_bytes(&o.raw).ok())
                        .and_then(|o| bare.commits.get(&o.to_text()).cloned())
                        .map(Value::String)
                        .unwrap_or(Value::Null)
                };
                let mut assignees = Vec::new();
                for a in &cr.assignees {
                    assignees.push(user(login_of(w, &Some(a.clone())).await, &mut users));
                }
                let author = user(login_of(w, &cr.author).await, &mut users);
                let landed = cr.state == v1::ChangeState::Landed as i32;
                let closed = cr.state == v1::ChangeState::Closed as i32;
                files.entry("pull_requests".into()).or_default().push(json!({
                    "type": "pull_request",
                    "url": url,
                    "repository": repo_url,
                    "user": author,
                    "title": cr.title,
                    "body": cr.body,
                    "draft": cr.draft,
                    "locked": cr.locked,
                    "labels": label_list(&cr.labels),
                    "assignees": assignees,
                    "milestone": cr.milestone.as_ref().map(|m| Value::String(ms_url(m.number))).unwrap_or(Value::Null),
                    "head": { "ref": format!("cr/{}", cr.number), "sha": sha_of(rev.and_then(|r| r.head.as_ref())), "repo": repo_url },
                    "base": { "ref": cr.target_ref, "sha": sha_of(rev.and_then(|r| r.base.as_ref())), "repo": repo_url },
                    "merged_at": if landed { iso(&cr.landed_at) } else { Value::Null },
                    "closed_at": if landed { iso(&cr.landed_at) } else if closed { iso(&cr.closed_at) } else { Value::Null },
                    "created_at": iso(&cr.created_at),
                    "updated_at": iso(&cr.updated_at),
                }));
                let mut ct = String::new();
                loop {
                    let (comments, next) = changes
                        .comments(&cx, &frepo, &cr.id, &ct, 500)
                        .await
                        .map_err(fe)?;
                    for c in &comments {
                        let u = user(login_of(w, &c.author).await, &mut users);
                        files
                            .entry("issue_comments".into())
                            .or_default()
                            .push(json!({
                                "type": "issue_comment",
                                "url": format!("{url}#issuecomment-{}", num_id(&c.id)),
                                "pull_request": url,
                                "user": u,
                                "body": c.body,
                                "created_at": iso(&c.created_at),
                                "updated_at": iso(&c.updated_at),
                            }));
                    }
                    if next.is_empty() {
                        break;
                    }
                    ct = next;
                }
                for r in changes.reviews(&cx, &frepo, &cr.id).await.map_err(fe)? {
                    let u = user(login_of(w, &r.reviewer).await, &mut users);
                    let state = match v1::Verdict::try_from(r.verdict) {
                        Ok(v1::Verdict::Approve) if !r.dismissed => "APPROVED",
                        Ok(v1::Verdict::RequestChanges) if !r.dismissed => "CHANGES_REQUESTED",
                        _ if r.dismissed => "DISMISSED",
                        _ => "COMMENTED",
                    };
                    files
                        .entry("pull_request_reviews".into())
                        .or_default()
                        .push(json!({
                            "type": "pull_request_review",
                            "url": format!("{url}/files#pullrequestreview-{}", num_id(&r.id)),
                            "pull_request": url,
                            "user": u,
                            "body": r.body,
                            "state": state,
                            "submitted_at": iso(&r.created_at),
                        }));
                }
            }
            if next.is_empty() {
                break;
            }
            token = next;
        }
    }
    // Releases.
    let mut token = String::new();
    loop {
        let (releases, next) = w
            .forge
            .community
            .releases
            .list(&cx, &frepo, true, true, &token, 200)
            .await
            .map_err(fe)?;
        for r in &releases {
            let u = user(login_of(w, &r.author).await, &mut users);
            files.entry("releases".into()).or_default().push(json!({
                "type": "release",
                "url": format!("{repo_url}/releases/{}", num_id(&r.id)),
                "repository": repo_url,
                "user": u,
                "tag_name": r.tag_name,
                "name": r.name,
                "body": r.body,
                "state": if r.draft { "draft" } else { "published" },
                "prerelease": r.prerelease,
                "target_commitish": r.target_ref,
                "created_at": iso(&r.created_at),
                "published_at": iso(&r.published_at),
            }));
        }
        if next.is_empty() {
            break;
        }
        token = next;
    }
    let all_users = files.entry("users".into()).or_default();
    for (_, u) in users {
        if !all_users.iter().any(|x| x["login"] == u["login"]) {
            all_users.push(u);
        }
    }
    Ok(())
}

/// `GetExport`.
pub async fn get(state: &AppState, caller: &Caller, id: &str) -> ApiResult<v1::Export> {
    let r = visible(state, caller, id).await?;
    with_url(state, caller, &r).await
}

async fn visible(state: &AppState, caller: &Caller, id: &str) -> ApiResult<ExportRec> {
    if id.is_empty() {
        return Err(ApiError::invalid("missing_field", "id is required"));
    }
    let r: ExportRec = get_json(state.kv.as_ref(), NS_EXPORT, id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("export {id}")))?;
    if r.principal != caller.id() {
        let org = crate::services::support::org_by_name_or_id(state, &r.org_id).await?;
        org_manager(state, caller, &org)
            .await
            .map_err(|_| ApiError::not_found(format!("export {id}")))?;
    }
    Ok(r)
}

/// `ListExports`.
pub async fn list(
    state: &AppState,
    caller: &Caller,
    req: v1::ListExportsRequest,
) -> ApiResult<v1::ListExportsResponse> {
    let org = crate::services::support::org_by_name_or_id(state, &req.org).await?;
    let manager = org_manager(state, caller, &org).await.is_ok();
    let org_id = org.id.0.to_text();
    let mut all: Vec<ExportRec> = scan_json::<ExportRec>(state.kv.as_ref(), NS_EXPORT, "")
        .await?
        .into_iter()
        .map(|(_, r)| r)
        .filter(|r| r.org_id == org_id && (manager || r.principal == caller.id()))
        .collect();
    all.sort_by(|a, b| b.started_ms.cmp(&a.started_ms).then(b.id.cmp(&a.id)));
    let (token, size) = page_of(&req.page);
    let (page, next) = offset_page(&all, &token, size)?;
    let mut exports = Vec::new();
    for r in &page {
        exports.push(with_url(state, caller, r).await?);
    }
    Ok(v1::ListExportsResponse {
        exports,
        page: next_page(next),
    })
}

/// `CancelExport`.
pub async fn cancel(
    state: &AppState,
    caller: &Caller,
    id: &str,
    meta: &RequestMeta,
) -> ApiResult<v1::Export> {
    visible(state, caller, id).await?;
    let now = state.now_ms();
    let r = update_rec(state, id, |cur| {
        let finished = cur.state == v1::ExportState::Succeeded as i32
            || cur.state == v1::ExportState::Failed as i32
            || cur.state == v1::ExportState::Cancelled as i32;
        (!finished).then(|| ExportRec {
            state: v1::ExportState::Cancelled as i32,
            finished_ms: now,
            ..cur.clone()
        })
    })
    .await?
    .ok_or_else(|| {
        ApiError::new(
            ErrorReason::InvalidState,
            format!("export {id} already finished"),
        )
    })?;
    state
        .audit(caller, meta, None, &r.id, "export.cancel", json!({}))
        .await?;
    Ok(proto(state, &r, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates_are_utc_and_numeric_ids_are_stable() {
        let t = Some(pbjson_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        });
        assert_eq!(iso(&t), Value::String("2023-11-14T22:13:20Z".into()));
        assert_eq!(iso(&None), Value::Null);
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(num_id("01J"), num_id("01J"));
        assert_ne!(num_id("01J"), num_id("01K"));
    }
}
