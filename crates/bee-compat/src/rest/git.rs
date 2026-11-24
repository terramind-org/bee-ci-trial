//! Contents, git data (refs, commits, trees, blobs), branches and protection, commits and
//! compare, archives. Every id is a git id from the [`crate::ports::GitPort`].

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bee_proto::v1::release_service_server::ReleaseService as _;
use bee_proto::v1::{ErrorReason, GetSourceArchiveUrlRequest, Repo, Revision, SourceArchiveFormat};
use serde_json::{Value, json};

use super::{b, need, repo_of, rid, s};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, encode};
use crate::ports::{
    EntryKind, FileChange, FileEdit, NewGitCommit, NewTreeItem, PathEntry, Signature,
};
use crate::shapes;

/// Where the repo policy lives (SPEC §17.2).
pub(crate) const POLICY_PATH: &str = ".beehive/policy.toml";

/// The policy written when a repo has none yet: paths stay open to everyone the forge's
/// roles admit (the root access rule every policy needs), land rules are added below it.
const DEFAULT_POLICY: &str =
    "version = 1\n\n[[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n";

fn default_branch(repo: &Repo) -> &str {
    if repo.default_ref.is_empty() {
        "main"
    } else {
        &repo.default_ref
    }
}

/// The commit `rev` names (`404` when nothing does).
pub(crate) async fn resolve(cx: &Ctx, repo: &Repo, rev: &str) -> ApiResult<String> {
    cx.hub.ports.git.resolve(repo, rev).await?.ok_or_else(|| {
        ApiError::new(
            404,
            ErrorReason::NotFound,
            format!("No commit found for the ref {rev}"),
        )
    })
}

/// The commit for `?ref=` or the default branch.
async fn at_ref(cx: &Ctx, repo: &Repo) -> ApiResult<String> {
    let rev = cx
        .q("ref")
        .filter(|r| !r.is_empty())
        .unwrap_or(default_branch(repo))
        .to_string();
    resolve(cx, repo, &rev).await
}

fn wrap_b64(bytes: &[u8]) -> String {
    let enc = STANDARD.encode(bytes);
    let mut out = String::with_capacity(enc.len() + enc.len() / 60 + 1);
    for chunk in enc.as_bytes().chunks(60) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out
}

fn entry_json(
    cx: &Ctx,
    repo: &Repo,
    rev: &str,
    path: &str,
    kind: &str,
    sha: &str,
    size: u64,
) -> Value {
    let name = path.rsplit('/').next().unwrap_or(path);
    let self_url = format!(
        "{}?ref={}",
        cx.api(&format!("/repos/{}/contents/{path}", repo.full_name)),
        encode(rev)
    );
    let git_url = match kind {
        "dir" => Value::String(cx.api(&format!("/repos/{}/git/trees/{sha}", repo.full_name))),
        "submodule" => Value::Null,
        _ => Value::String(cx.api(&format!("/repos/{}/git/blobs/{sha}", repo.full_name))),
    };
    let html = cx.html(&format!(
        "/{}/{}/{rev}/{path}",
        repo.full_name,
        if kind == "dir" { "tree" } else { "blob" }
    ));
    let download = if kind == "file" || kind == "symlink" {
        Value::String(cx.html(&format!("/{}/raw/{rev}/{path}", repo.full_name)))
    } else {
        Value::Null
    };
    json!({
        "type": kind,
        "size": size,
        "name": name,
        "path": path,
        "sha": sha,
        "url": self_url,
        "git_url": git_url,
        "html_url": html,
        "download_url": download,
        "_links": {"self": self_url, "git": git_url, "html": html},
    })
}

fn kind_word(k: EntryKind, mode: &str) -> &'static str {
    match k {
        EntryKind::Tree => "dir",
        EntryKind::Commit => "submodule",
        EntryKind::Blob if mode == "120000" => "symlink",
        EntryKind::Blob => "file",
    }
}

async fn contents_at(
    cx: &Ctx,
    repo: &Repo,
    rev_name: &str,
    commit: &str,
    path: &str,
) -> ApiResult<Reply> {
    let entry = cx
        .hub
        .ports
        .git
        .path(repo, commit, path)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let raw = cx.accepts("raw");
    Ok(match entry {
        PathEntry::File { sha, bytes } => {
            if raw {
                return Ok(Reply::raw(bytes, "application/vnd.github.raw"));
            }
            let mut v = entry_json(cx, repo, rev_name, path, "file", &sha, bytes.len() as u64);
            v["content"] = json!(wrap_b64(&bytes));
            v["encoding"] = json!("base64");
            Reply::json(v)
        }
        PathEntry::Symlink { sha, target } => {
            let mut v = entry_json(
                cx,
                repo,
                rev_name,
                path,
                "symlink",
                &sha,
                target.len() as u64,
            );
            v["target"] = json!(target);
            Reply::json(v)
        }
        PathEntry::Submodule { sha } => {
            let mut v = entry_json(cx, repo, rev_name, path, "submodule", &sha, 0);
            v["submodule_git_url"] = Value::Null;
            Reply::json(v)
        }
        PathEntry::Dir { entries, .. } => {
            let items = entries
                .iter()
                .map(|e| {
                    let full = if path.is_empty() {
                        e.path.clone()
                    } else {
                        format!("{path}/{}", e.path)
                    };
                    entry_json(
                        cx,
                        repo,
                        rev_name,
                        &full,
                        kind_word(e.kind, &e.mode),
                        &e.sha,
                        e.size.unwrap_or(0),
                    )
                })
                .collect();
            Reply::json(Value::Array(items))
        }
    })
}

/// `GET /repos/{owner}/{repo}/contents/{path}`.
pub(crate) async fn contents(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let commit = at_ref(&cx, &repo).await?;
    let rev = cx.q("ref").unwrap_or(default_branch(&repo)).to_string();
    contents_at(&cx, &repo, &rev, &commit, p.get("path").trim_matches('/')).await
}

/// `GET /repos/{owner}/{repo}/readme[/{dir}]`.
pub(crate) async fn readme(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let commit = at_ref(&cx, &repo).await?;
    let rev = cx.q("ref").unwrap_or(default_branch(&repo)).to_string();
    let dir = p.get("dir").trim_matches('/');
    let Some(PathEntry::Dir { entries, .. }) = cx.hub.ports.git.path(&repo, &commit, dir).await?
    else {
        return Err(ApiError::not_found());
    };
    let mut names: Vec<&str> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::Blob)
        .map(|e| e.path.as_str())
        .filter(|n| n.to_lowercase().starts_with("readme"))
        .collect();
    names.sort_by_key(|n| (n.to_lowercase() != "readme.md", n.len()));
    let name = names.first().ok_or_else(ApiError::not_found)?;
    let path = if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    };
    contents_at(&cx, &repo, &rev, &commit, &path).await
}

fn signature_of(v: Option<&Value>) -> Option<Signature> {
    let v = v?;
    Some(Signature {
        name: s(v, "name")?.to_string(),
        email: s(v, "email").unwrap_or("").to_string(),
        time_secs: s(v, "date").and_then(crate::time::parse_iso).unwrap_or(0),
    })
}

/// Fails with `422 POLICY_DENIED` when `branch` is protected: protected branches move only
/// through Land (a pull request merge).
async fn guard_protected(cx: &Ctx, repo: &Repo, branch: &str) -> ApiResult<()> {
    if let Some(src) = &cx.hub.forge.hooks.policy
        && let Some(policy) = src.policy(repo).await?
        && policy.is_protected(branch)
    {
        return Err(ApiError::new(
            422,
            ErrorReason::PolicyDenied,
            format!(
                "Protected branch update failed for refs/heads/{branch}: changes must be made through a pull request."
            ),
        ));
    }
    Ok(())
}

async fn write_file(cx: Arc<Ctx>, p: Params, delete: bool) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let path = p.get("path").trim_matches('/').to_string();
    let message = need(&body, "message", "Contents")?.to_string();
    let branch = s(&body, "branch")
        .unwrap_or(default_branch(&repo))
        .to_string();
    let git = &cx.hub.ports.git;
    let tip = git.resolve(&repo, &branch).await?;
    if tip.is_none() && s(&body, "branch").is_some() && delete {
        return Err(ApiError::not_found().with_message(format!("Branch {branch} not found")));
    }
    let existing = match &tip {
        Some(t) => git.path(&repo, t, &path).await?,
        None => None,
    };
    let existing_sha = match &existing {
        Some(PathEntry::File { sha, .. }) | Some(PathEntry::Symlink { sha, .. }) => {
            Some(sha.clone())
        }
        Some(_) => return Err(ApiError::unprocessable(format!("{path} is a directory"))),
        None => None,
    };
    let given = s(&body, "sha");
    match (&existing_sha, given) {
        (Some(_), None) => {
            return Err(ApiError::validation(
                "Contents",
                "sha",
                "missing_field",
                "\"sha\" wasn't supplied.",
            ));
        }
        (Some(cur), Some(g)) if cur != g => {
            return Err(ApiError::new(
                409,
                ErrorReason::VersionMismatch,
                format!("{path} does not match {g}"),
            ));
        }
        (None, _) if delete => return Err(ApiError::not_found()),
        _ => {}
    }
    let content = if delete {
        None
    } else {
        let b64 = need(&body, "content", "Contents")?;
        let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        Some(STANDARD.decode(clean).map_err(|_| {
            ApiError::validation(
                "Contents",
                "content",
                "invalid",
                "content is not valid Base64",
            )
        })?)
    };
    guard_protected(&cx, &repo, &branch).await?;
    let actor = cx.actor().await?;
    let sha = git
        .commit_files(
            &repo,
            &actor,
            &branch,
            tip.as_deref(),
            vec![FileEdit {
                path: path.clone(),
                content,
            }],
            &message,
        )
        .await?;
    let commit = git
        .commit(&repo, &sha)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let content_json = if delete {
        Value::Null
    } else {
        match git.path(&repo, &sha, &path).await? {
            Some(PathEntry::File { sha: fsha, bytes }) => entry_json(
                &cx,
                &repo,
                &branch,
                &path,
                "file",
                &fsha,
                bytes.len() as u64,
            ),
            _ => Value::Null,
        }
    };
    let out = json!({"content": content_json, "commit": shapes::git_commit(&cx, &repo, &commit)});
    Ok(if existing_sha.is_none() {
        Reply::created(out)
    } else {
        Reply::json(out)
    })
}

/// `PUT /repos/{owner}/{repo}/contents/{path}`.
pub(crate) async fn put_contents(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    write_file(cx, p, false).await
}

/// `DELETE /repos/{owner}/{repo}/contents/{path}`.
pub(crate) async fn delete_contents(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    write_file(cx, p, true).await
}

async fn archive(cx: Arc<Ctx>, p: Params, format: SourceArchiveFormat) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let rev = match p.get("ref") {
        "" => default_branch(&repo).to_string(),
        r => r.to_string(),
    };
    let sha = resolve(&cx, &repo, &rev).await?;
    let cs = cx
        .hub
        .ports
        .git
        .changeset(&repo, &sha)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let hex = super::revision_text(&cs)?;
    let r = cx
        .hub
        .svc
        .get_source_archive_url(cx.req(GetSourceArchiveUrlRequest {
            repo: rid(&repo.id),
            at: Some(Revision { expr: hex }),
            format: format as i32,
            prefix: String::new(),
        }))
        .await?
        .into_inner();
    Ok(Reply::redirect(&r.url))
}

/// `GET /repos/{owner}/{repo}/tarball/{ref}`: a redirect to the signed archive URL.
pub(crate) async fn tarball(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    archive(cx, p, SourceArchiveFormat::TarGz).await
}

/// `GET /repos/{owner}/{repo}/zipball/{ref}`.
pub(crate) async fn zipball(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    archive(cx, p, SourceArchiveFormat::Zip).await
}

fn ref_json(cx: &Ctx, repo: &Repo, name: &str, sha: &str) -> Value {
    json!({
        "ref": name,
        "node_id": crate::ids::node("Ref", &format!("{}:{name}", repo.id)),
        "url": cx.api(&format!("/repos/{}/git/{name}", repo.full_name)),
        "object": {
            "type": "commit",
            "sha": sha,
            "url": cx.api(&format!("/repos/{}/git/commits/{sha}", repo.full_name)),
        },
    })
}

/// `GET /repos/{owner}/{repo}/git/refs[/{ref}]`: one ref on an exact match, else every ref
/// under the prefix.
pub(crate) async fn refs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let want = p.get("ref");
    let full = if want.is_empty() {
        "refs/".to_string()
    } else {
        format!("refs/{want}")
    };
    let all = cx.hub.ports.git.refs(&repo, &full).await?;
    if let Some(r) = all.iter().find(|r| r.name == full) {
        return Ok(Reply::json(ref_json(&cx, &repo, &r.name, &r.sha)));
    }
    let prefix = if full.ends_with('/') {
        full.clone()
    } else {
        format!("{full}/")
    };
    let items: Vec<Value> = all
        .iter()
        .filter(|r| r.name.starts_with(&prefix))
        .map(|r| ref_json(&cx, &repo, &r.name, &r.sha))
        .collect();
    if items.is_empty() {
        return Err(ApiError::not_found());
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/git/matching-refs/{ref}`.
pub(crate) async fn matching_refs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let full = format!("refs/{}", p.get("ref"));
    let items = cx
        .hub
        .ports
        .git
        .refs(&repo, &full)
        .await?
        .iter()
        .map(|r| ref_json(&cx, &repo, &r.name, &r.sha))
        .collect();
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/git/ref/{ref}`.
pub(crate) async fn get_ref(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let full = format!("refs/{}", p.get("ref"));
    let r = cx
        .hub
        .ports
        .git
        .refs(&repo, &full)
        .await?
        .into_iter()
        .find(|r| r.name == full)
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::json(ref_json(&cx, &repo, &r.name, &r.sha)))
}

fn branch_of(name: &str) -> Option<&str> {
    name.strip_prefix("refs/heads/")
}

/// `POST /repos/{owner}/{repo}/git/refs`.
pub(crate) async fn create_ref(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let name = need(&body, "ref", "Reference")?.to_string();
    let sha = need(&body, "sha", "Reference")?.to_string();
    if !name.starts_with("refs/") || name.matches('/').count() < 2 {
        return Err(ApiError::unprocessable(
            "Reference name must start with 'refs/' and have at least two slashes.",
        ));
    }
    let git = &cx.hub.ports.git;
    if git.refs(&repo, &name).await?.iter().any(|r| r.name == name) {
        return Err(ApiError::unprocessable("Reference already exists"));
    }
    let actor = cx.actor().await?;
    git.update_ref(&repo, &actor, &name, Some(&sha), None, true)
        .await?;
    Ok(Reply::created(ref_json(&cx, &repo, &name, &sha)))
}

/// `PATCH /repos/{owner}/{repo}/git/refs/{ref}`.
pub(crate) async fn update_ref(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let sha = need(&body, "sha", "Reference")?.to_string();
    let name = format!("refs/{}", p.get("ref"));
    if let Some(br) = branch_of(&name) {
        guard_protected(&cx, &repo, br).await?;
    }
    let actor = cx.actor().await?;
    cx.hub
        .ports
        .git
        .update_ref(
            &repo,
            &actor,
            &name,
            Some(&sha),
            None,
            b(&body, "force").unwrap_or(false),
        )
        .await?;
    Ok(Reply::json(ref_json(&cx, &repo, &name, &sha)))
}

/// `DELETE /repos/{owner}/{repo}/git/refs/{ref}`.
pub(crate) async fn delete_ref(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let name = format!("refs/{}", p.get("ref"));
    if let Some(br) = branch_of(&name) {
        if br == default_branch(&repo) {
            return Err(ApiError::unprocessable("Cannot delete the default branch"));
        }
        guard_protected(&cx, &repo, br).await?;
    }
    let actor = cx.actor().await?;
    cx.hub
        .ports
        .git
        .update_ref(&repo, &actor, &name, None, None, true)
        .await?;
    Ok(Reply::no_content())
}

/// `GET /repos/{owner}/{repo}/git/commits/{sha}`.
pub(crate) async fn git_commit(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cx
        .hub
        .ports
        .git
        .commit(&repo, p.get("sha"))
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::json(shapes::git_commit(&cx, &repo, &c)))
}

/// `POST /repos/{owner}/{repo}/git/commits`.
pub(crate) async fn create_commit(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let message = need(&body, "message", "Commit")?.to_string();
    let tree = need(&body, "tree", "Commit")?.to_string();
    let parents = super::strs(&body, "parents").unwrap_or_default();
    let actor = cx.actor().await?;
    let git = &cx.hub.ports.git;
    let sha = git
        .create_commit(
            &repo,
            &actor,
            NewGitCommit {
                message,
                tree,
                parents,
                author: signature_of(body.get("author")),
            },
        )
        .await?;
    let c = git
        .commit(&repo, &sha)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::created(shapes::git_commit(&cx, &repo, &c)))
}

/// `GET /repos/{owner}/{repo}/git/trees/{sha}?recursive=1`.
pub(crate) async fn tree(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let git = &cx.hub.ports.git;
    let want = p.get("sha");
    let sha = match git.tree(&repo, want, false).await? {
        Some(_) => want.to_string(),
        None => resolve(&cx, &repo, want).await?,
    };
    let recursive = cx
        .q("recursive")
        .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
    let t = git
        .tree(&repo, &sha, recursive)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::json(tree_json(&cx, &repo, &t)))
}

fn tree_json(cx: &Ctx, repo: &Repo, t: &crate::ports::TreeListing) -> Value {
    let base = cx.api(&format!("/repos/{}/git", repo.full_name));
    let items: Vec<Value> = t
        .entries
        .iter()
        .map(|e| {
            let mut v = json!({
                "path": e.path, "mode": e.mode, "type": e.kind.as_str(), "sha": e.sha,
                "url": format!("{base}/{}s/{}", if e.kind == EntryKind::Tree { "tree" } else { "blob" }, e.sha),
            });
            if let Some(sz) = e.size {
                v["size"] = json!(sz);
            }
            if e.kind == EntryKind::Commit {
                v.as_object_mut().map(|m| m.remove("url"));
            }
            v
        })
        .collect();
    json!({
        "sha": t.sha,
        "url": format!("{base}/trees/{}", t.sha),
        "tree": items,
        "truncated": t.truncated,
    })
}

/// `POST /repos/{owner}/{repo}/git/trees`.
pub(crate) async fn create_tree(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let entries = body
        .get("tree")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::missing_field("Tree", "tree"))?;
    let mut items = Vec::new();
    for e in entries {
        let path = need(e, "path", "Tree")?.to_string();
        let mode = s(e, "mode").unwrap_or("100644").to_string();
        let sha = match e.get("sha") {
            Some(Value::String(x)) => Some(x.clone()),
            _ => None,
        };
        let content = s(e, "content").map(|c| c.as_bytes().to_vec());
        if sha.is_some() && content.is_some() {
            return Err(ApiError::validation(
                "Tree",
                "content",
                "invalid",
                "only one of sha and content may be given",
            ));
        }
        if sha.is_none() && content.is_none() && !matches!(e.get("sha"), Some(Value::Null)) {
            return Err(ApiError::missing_field("Tree", "sha"));
        }
        items.push(NewTreeItem {
            path,
            mode,
            sha,
            content,
        });
    }
    let actor = cx.actor().await?;
    let git = &cx.hub.ports.git;
    let sha = git
        .create_tree(&repo, &actor, s(&body, "base_tree"), items)
        .await?;
    let t = git
        .tree(&repo, &sha, false)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::created(tree_json(&cx, &repo, &t)))
}

/// `GET /repos/{owner}/{repo}/git/blobs/{sha}`.
pub(crate) async fn blob(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sha = p.get("sha");
    let bytes = cx
        .hub
        .ports
        .git
        .blob(&repo, sha)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if cx.accepts("raw") {
        return Ok(Reply::raw(bytes, "application/vnd.github.raw"));
    }
    Ok(Reply::json(json!({
        "sha": sha,
        "node_id": crate::ids::node("Blob", &format!("{}:{sha}", repo.id)),
        "size": bytes.len(),
        "url": cx.api(&format!("/repos/{}/git/blobs/{sha}", repo.full_name)),
        "content": wrap_b64(&bytes),
        "encoding": "base64",
    })))
}

/// `POST /repos/{owner}/{repo}/git/blobs`.
pub(crate) async fn create_blob(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::missing_field("Blob", "content"))?;
    let bytes = match s(&body, "encoding").unwrap_or("utf-8") {
        "base64" => STANDARD
            .decode(
                content
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
            )
            .map_err(|_| {
                ApiError::validation("Blob", "content", "invalid", "content is not valid Base64")
            })?,
        "utf-8" | "utf8" => content.as_bytes().to_vec(),
        other => return Err(ApiError::validation("Blob", "encoding", "invalid", other)),
    };
    let actor = cx.actor().await?;
    let sha = cx.hub.ports.git.create_blob(&repo, &actor, bytes).await?;
    Ok(Reply::created(json!({
        "sha": sha,
        "url": cx.api(&format!("/repos/{}/git/blobs/{sha}", repo.full_name)),
    })))
}

/// The protected-branch view of the repo policy for `branch`.
pub(crate) async fn protection_of(cx: &Ctx, repo: &Repo, branch: &str) -> ApiResult<Option<Value>> {
    let Some(src) = &cx.hub.forge.hooks.policy else {
        return Ok(None);
    };
    let Some(policy) = src.policy(repo).await? else {
        return Ok(None);
    };
    let doc = policy.doc();
    let best = doc
        .land
        .iter()
        .filter(|(pat, _)| pat.matches(branch))
        .max_by_key(|(pat, _)| match pat {
            bee_policy::RefPattern::Exact(_) => usize::MAX,
            bee_policy::RefPattern::Prefix(p) => p.len(),
        });
    let Some((_, rule)) = best else {
        return Ok(policy
            .is_protected(branch)
            .then(|| protection_json(cx, repo, branch, None)));
    };
    Ok(Some(protection_json(cx, repo, branch, Some(&rule.body))))
}

fn protection_json(
    cx: &Ctx,
    repo: &Repo,
    branch: &str,
    body: Option<&bee_policy::RuleBody>,
) -> Value {
    let url = cx.api(&format!(
        "/repos/{}/branches/{}/protection",
        repo.full_name,
        encode(branch)
    ));
    let flag = |on: bool| json!({"url": url, "enabled": on});
    let Some(body) = body else {
        return json!({"url": url, "enforce_admins": flag(true), "required_linear_history": flag(false),
            "allow_force_pushes": flag(false), "allow_deletions": flag(false), "required_signatures": flag(false),
            "required_conversation_resolution": flag(false), "lock_branch": flag(false), "allow_fork_syncing": flag(false)});
    };
    let contexts: Vec<&String> = body.required_checks.iter().collect();
    let count = body.approvals.iter().map(|a| a.count).max().unwrap_or(0);
    let owners = body
        .approvals
        .iter()
        .any(|a| matches!(a.from, bee_policy::ApproverSet::Owners));
    let mut v = json!({
        "url": url,
        "required_status_checks": {
            "url": format!("{url}/required_status_checks"),
            "strict": true,
            "contexts": contexts,
            "checks": contexts.iter().map(|c| json!({"context": c, "app_id": Value::Null})).collect::<Vec<_>>(),
            "contexts_url": format!("{url}/required_status_checks/contexts"),
        },
        "enforce_admins": flag(true),
        "required_linear_history": flag(body.require_linear),
        "allow_force_pushes": flag(false),
        "allow_deletions": flag(false),
        "block_creations": flag(false),
        "required_conversation_resolution": flag(false),
        "lock_branch": flag(body.allow.as_ref().is_some_and(|a| a.is_empty())),
        "allow_fork_syncing": flag(false),
        "required_signatures": flag(body.require_signed),
    });
    if count > 0 || body.dismiss_stale_approvals || body.require_last_push_approval {
        v["required_pull_request_reviews"] = json!({
            "url": format!("{url}/required_pull_request_reviews"),
            "dismiss_stale_reviews": body.dismiss_stale_approvals,
            "require_code_owner_reviews": owners,
            "required_approving_review_count": count,
            "require_last_push_approval": body.require_last_push_approval,
        });
    }
    v
}

fn branch_json(cx: &Ctx, repo: &Repo, name: &str, sha: &str, protected: bool) -> Value {
    json!({
        "name": name,
        "commit": {"sha": sha, "url": cx.api(&format!("/repos/{}/commits/{sha}", repo.full_name))},
        "protected": protected,
    })
}

/// `GET /repos/{owner}/{repo}/branches`.
pub(crate) async fn branches(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let refs = cx.hub.ports.git.refs(&repo, "refs/heads/").await?;
    let policy = match &cx.hub.forge.hooks.policy {
        Some(src) => src.policy(&repo).await?,
        None => None,
    };
    let only_protected = cx.q("protected") == Some("true");
    let items = refs
        .iter()
        .filter_map(|r| {
            let name = r.name.trim_start_matches("refs/heads/");
            let prot = policy.as_ref().is_some_and(|p| p.is_protected(name));
            (!only_protected || prot).then(|| branch_json(&cx, &repo, name, &r.sha, prot))
        })
        .collect();
    Ok(cx.paginate(items))
}

/// Splits `branches/<name>[/protection[/...]]`.
fn split_branch(rest: &str) -> (&str, &str) {
    match rest.find("/protection") {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => match rest.strip_suffix("/rename") {
            Some(b) => (b, "rename"),
            None => (rest, ""),
        },
    }
}

/// `GET /repos/{owner}/{repo}/branches/{branch}[/protection...]`.
pub(crate) async fn branch_get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (name, sub) = split_branch(p.get("rest"));
    let sha = cx
        .hub
        .ports
        .git
        .refs(&repo, &format!("refs/heads/{name}"))
        .await?
        .into_iter()
        .find(|r| r.name == format!("refs/heads/{name}"))
        .map(|r| r.sha)
        .ok_or_else(|| ApiError::not_found().with_message("Branch not found"))?;
    let prot = protection_of(&cx, &repo, name).await?;
    match sub {
        "" => {
            let c = cx
                .hub
                .ports
                .git
                .commit(&repo, &sha)
                .await?
                .ok_or_else(ApiError::not_found)?;
            let url = cx.api(&format!(
                "/repos/{}/branches/{}",
                repo.full_name,
                encode(name)
            ));
            Ok(Reply::json(json!({
                "name": name,
                "commit": shapes::commit(&cx, &repo, &c).await,
                "_links": {"self": url, "html": cx.html(&format!("/{}/tree/{name}", repo.full_name))},
                "protected": prot.is_some(),
                "protection": {
                    "enabled": prot.is_some(),
                    "required_status_checks": {
                        "enforcement_level": if prot.is_some() { "everyone" } else { "off" },
                        "contexts": prot.as_ref().and_then(|v| v.pointer("/required_status_checks/contexts").cloned()).unwrap_or(json!([])),
                        "checks": prot.as_ref().and_then(|v| v.pointer("/required_status_checks/checks").cloned()).unwrap_or(json!([])),
                    },
                },
                "protection_url": format!("{url}/protection"),
            })))
        }
        "protection" => prot
            .map(Reply::json)
            .ok_or_else(|| ApiError::not_found().with_message("Branch not protected")),
        sub => {
            let key = sub.trim_start_matches("protection/");
            let v =
                prot.ok_or_else(|| ApiError::not_found().with_message("Branch not protected"))?;
            match key {
                "required_status_checks/contexts" => Ok(Reply::json(
                    v.pointer("/required_status_checks/contexts")
                        .cloned()
                        .unwrap_or(json!([])),
                )),
                k => v
                    .get(k)
                    .cloned()
                    .map(Reply::json)
                    .ok_or_else(ApiError::not_found),
            }
        }
    }
}

/// Removes `[[land]]` blocks for `branch` from a policy file.
fn strip_land(text: &str, branch: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    let mut in_land = false;
    let want = |l: &str| {
        let t = l.trim();
        t.starts_with("ref")
            && t.split_once('=')
                .is_some_and(|(_, v)| v.trim().trim_matches('"').trim_matches('\'') == branch)
    };
    fn flush<'a>(block: &mut Vec<&'a str>, out: &mut Vec<&'a str>, want: &dyn Fn(&str) -> bool) {
        if !block.iter().any(|l| want(l)) {
            out.append(block);
        }
        block.clear();
    }
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            let land_header = t == "[[land]]";
            let land_child = t.starts_with("[[land.") || t.starts_with("[land.");
            if land_header {
                if in_land {
                    flush(&mut block, &mut out, &want);
                }
                in_land = true;
                block.push(line);
                continue;
            }
            if in_land && !land_child {
                flush(&mut block, &mut out, &want);
                in_land = false;
            }
        }
        if in_land {
            block.push(line);
        } else {
            out.push(line);
        }
    }
    if in_land {
        flush(&mut block, &mut out, &want);
    }
    let mut s = out.join("\n");
    while s.ends_with("\n\n") {
        s.pop();
    }
    s
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The `[[land]]` block GitHub's protection body asks for.
fn land_block(branch: &str, body: &Value) -> String {
    let mut out = format!("\n[[land]]\nref = {}\n", toml_str(branch));
    let checks: Vec<String> = body
        .pointer("/required_status_checks/contexts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(toml_str).collect())
        .or_else(|| {
            body.pointer("/required_status_checks/checks")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|c| s(c, "context"))
                        .map(toml_str)
                        .collect()
                })
        })
        .unwrap_or_default();
    if !checks.is_empty() {
        out.push_str(&format!("required_checks = [{}]\n", checks.join(", ")));
    }
    let reviews = body
        .get("required_pull_request_reviews")
        .filter(|v| v.is_object());
    if let Some(r) = reviews {
        if b(r, "dismiss_stale_reviews") == Some(true) {
            out.push_str("dismiss_stale_approvals = true\n");
        }
        if b(r, "require_last_push_approval") == Some(true) {
            out.push_str("require_last_push_approval = true\n");
        }
    }
    if b(body, "required_linear_history") == Some(true) {
        out.push_str("require_linear = true\n");
    }
    if b(body, "required_signatures") == Some(true) {
        out.push_str("require_signed = true\n");
    }
    if b(body, "lock_branch") == Some(true) {
        out.push_str("allow = []\n");
    }
    if let Some(r) = reviews {
        let count = super::u(r, "required_approving_review_count").unwrap_or(1);
        if count > 0 {
            out.push_str(&format!("\n[[land.approvals]]\ncount = {count}\n"));
            if b(r, "require_code_owner_reviews") == Some(true) {
                out.push_str("from = \"owners\"\n");
            }
        }
    }
    out
}

async fn write_policy(cx: &Ctx, repo: &Repo, branch: &str, block: Option<String>) -> ApiResult<()> {
    let git = &cx.hub.ports.git;
    let main = default_branch(repo).to_string();
    let tip = git.resolve(repo, &main).await?;
    let current = match &tip {
        Some(t) => match git.path(repo, t, POLICY_PATH).await? {
            Some(PathEntry::File { bytes, .. }) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => String::new(),
        },
        None => String::new(),
    };
    let mut text = if current.trim().is_empty() {
        DEFAULT_POLICY.to_string()
    } else {
        strip_land(&current, branch)
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if let Some(b) = block {
        text.push_str(&b);
    }
    bee_policy::Policy::parse(&text).map_err(|e| {
        ApiError::validation("BranchProtection", "policy", "invalid", &e.to_string())
    })?;
    if text == current {
        return Ok(());
    }
    let actor = cx.actor().await?;
    git.commit_files(
        repo,
        &actor,
        &main,
        tip.as_deref(),
        vec![FileEdit {
            path: POLICY_PATH.into(),
            content: Some(text.into_bytes()),
        }],
        &format!("Update land rules for {branch}"),
    )
    .await?;
    Ok(())
}

/// `PUT /repos/{owner}/{repo}/branches/{branch}/protection`: writes the land rule to
/// `.beehive/policy.toml` on the default branch.
pub(crate) async fn branch_put(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (name, sub) = split_branch(p.get("rest"));
    if sub != "protection" {
        return Err(ApiError::not_found());
    }
    let body = cx.body_json()?;
    write_policy(&cx, &repo, name, Some(land_block(name, &body))).await?;
    let block = land_block(name, &body);
    let parsed = bee_policy::Policy::parse(&format!("{DEFAULT_POLICY}{block}")).map_err(|e| {
        ApiError::validation("BranchProtection", "policy", "invalid", &e.to_string())
    })?;
    let rule = parsed.doc().land.values().next().map(|r| r.body.clone());
    Ok(Reply::json(protection_json(
        &cx,
        &repo,
        name,
        rule.as_ref(),
    )))
}

/// `DELETE /repos/{owner}/{repo}/branches/{branch}/protection`.
pub(crate) async fn branch_delete(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (name, sub) = split_branch(p.get("rest"));
    if sub != "protection" {
        return Err(ApiError::not_found());
    }
    write_policy(&cx, &repo, name, None).await?;
    Ok(Reply::no_content())
}

/// `POST /repos/{owner}/{repo}/branches/{branch}/rename`.
pub(crate) async fn branch_post(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (name, sub) = split_branch(p.get("rest"));
    if sub != "rename" {
        return Err(ApiError::not_found());
    }
    let body = cx.body_json()?;
    let new = need(&body, "new_name", "Branch")?.to_string();
    let git = &cx.hub.ports.git;
    let old_ref = format!("refs/heads/{name}");
    let sha = git
        .refs(&repo, &old_ref)
        .await?
        .into_iter()
        .find(|r| r.name == old_ref)
        .map(|r| r.sha)
        .ok_or_else(ApiError::not_found)?;
    guard_protected(&cx, &repo, name).await?;
    let actor = cx.actor().await?;
    git.update_ref(
        &repo,
        &actor,
        &format!("refs/heads/{new}"),
        Some(&sha),
        None,
        true,
    )
    .await?;
    git.update_ref(&repo, &actor, &old_ref, None, None, true)
        .await?;
    Ok(Reply::created(branch_json(&cx, &repo, &new, &sha, false)))
}

/// `GET /repos/{owner}/{repo}/commits`.
pub(crate) async fn commits(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let rev = cx
        .q("sha")
        .filter(|s| !s.is_empty())
        .unwrap_or(default_branch(&repo))
        .to_string();
    let Some(head) = cx.hub.ports.git.resolve(&repo, &rev).await? else {
        if cx
            .hub
            .ports
            .git
            .refs(&repo, "refs/heads/")
            .await?
            .is_empty()
        {
            return Err(ApiError::new(
                409,
                ErrorReason::NotFound,
                "Git Repository is empty.",
            ));
        }
        return Err(ApiError::not_found());
    };
    let (page, per) = cx.page();
    let since = cx.q("since").and_then(crate::time::parse_iso);
    let until = cx.q("until").and_then(crate::time::parse_iso);
    let author = cx.q("author");
    let filtered = since.is_some() || until.is_some() || author.is_some();
    let path = cx.q("path").filter(|p| !p.is_empty());
    let git = &cx.hub.ports.git;
    let mut list = if filtered {
        git.log(&repo, &head, path, 0, crate::http::LIST_CAP)
            .await?
    } else {
        git.log(&repo, &head, path, (page - 1) * per, per + 1)
            .await?
    };
    if filtered {
        list.retain(|c| {
            since.is_none_or(|s| c.committer.time_secs >= s)
                && until.is_none_or(|u| c.committer.time_secs <= u)
                && author.is_none_or(|a| c.author.name == a || c.author.email == a)
        });
        let mut items = Vec::new();
        for c in &list {
            items.push(shapes::commit(&cx, &repo, c).await);
        }
        return Ok(cx.paginate(items));
    }
    let more = list.len() > per;
    list.truncate(per);
    let mut items = Vec::new();
    for c in &list {
        items.push(shapes::commit(&cx, &repo, c).await);
    }
    let mut reply = Reply::json(Value::Array(items));
    if more || page > 1 {
        let url = |n: usize| {
            let mut q: Vec<String> = cx
                .query
                .iter()
                .filter(|(k, _)| k != "page")
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect();
            q.push(format!("page={n}"));
            format!("<{}?{}>", cx.api(&cx.path), q.join("&"))
        };
        let mut parts = Vec::new();
        if page > 1 {
            parts.push(format!("{}; rel=\"prev\"", url(page - 1)));
        }
        if more {
            parts.push(format!("{}; rel=\"next\"", url(page + 1)));
        }
        if page > 1 {
            parts.push(format!("{}; rel=\"first\"", url(1)));
        }
        reply = reply.header("link", parts.join(", "));
    }
    Ok(reply)
}

/// A changed file in GitHub's shape.
pub(crate) fn file_json(cx: &Ctx, repo: &Repo, head: &str, f: &FileChange) -> Value {
    let mut v = json!({
        "sha": f.sha,
        "filename": f.filename,
        "status": f.status,
        "additions": f.additions,
        "deletions": f.deletions,
        "changes": f.additions + f.deletions,
        "blob_url": cx.html(&format!("/{}/blob/{head}/{}", repo.full_name, f.filename)),
        "raw_url": cx.html(&format!("/{}/raw/{head}/{}", repo.full_name, f.filename)),
        "contents_url": format!("{}?ref={head}", cx.api(&format!("/repos/{}/contents/{}", repo.full_name, f.filename))),
    });
    if let Some(p) = &f.patch {
        v["patch"] = json!(p);
    }
    if let Some(prev) = &f.previous_filename {
        v["previous_filename"] = json!(prev);
    }
    v
}

/// Unified diff text for `files` (`Accept: application/vnd.github.diff`).
pub(crate) fn diff_text(files: &[FileChange]) -> String {
    let mut out = String::new();
    for f in files {
        let old = f.previous_filename.as_deref().unwrap_or(&f.filename);
        out.push_str(&format!("diff --git a/{old} b/{}\n", f.filename));
        match f.status.as_str() {
            "added" => out.push_str("new file mode 100644\n"),
            "removed" => out.push_str("deleted file mode 100644\n"),
            _ => {}
        }
        let (a, bn) = match f.status.as_str() {
            "added" => ("/dev/null".to_string(), format!("b/{}", f.filename)),
            "removed" => (format!("a/{old}"), "/dev/null".to_string()),
            _ => (format!("a/{old}"), format!("b/{}", f.filename)),
        };
        match &f.patch {
            Some(p) if !p.is_empty() => {
                out.push_str(&format!("--- {a}\n+++ {bn}\n{p}\n"));
            }
            Some(_) => {}
            None => out.push_str(&format!("Binary files {a} and {bn} differ\n")),
        }
    }
    out
}

/// `GET /repos/{owner}/{repo}/commits/{ref}`.
pub(crate) async fn commit(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sha = resolve(&cx, &repo, p.get("ref")).await?;
    let git = &cx.hub.ports.git;
    let c = git
        .commit(&repo, &sha)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let files = match c.parents.first() {
        Some(parent) => git.diff(&repo, parent, &sha).await?,
        None => Vec::new(),
    };
    if cx.accepts("diff") || cx.accepts("patch") {
        return Ok(Reply::raw(diff_text(&files), "text/plain; charset=utf-8"));
    }
    let mut v = shapes::commit(&cx, &repo, &c).await;
    let adds: u64 = files.iter().map(|f| f.additions).sum();
    let dels: u64 = files.iter().map(|f| f.deletions).sum();
    v["stats"] = json!({"total": adds + dels, "additions": adds, "deletions": dels});
    v["files"] = Value::Array(
        files
            .iter()
            .map(|f| file_json(&cx, &repo, &sha, f))
            .collect(),
    );
    Ok(Reply::json(v))
}

/// `GET /repos/{owner}/{repo}/compare/{base}...{head}`.
pub(crate) async fn compare(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let spec = p.get("basehead");
    let (base, head) = spec
        .split_once("...")
        .or_else(|| spec.split_once(".."))
        .ok_or_else(ApiError::not_found)?;
    let strip = |r: &str| {
        r.rsplit_once(':')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| r.to_string())
    };
    let (base, head) = (strip(base), strip(head));
    let bsha = resolve(&cx, &repo, &base).await?;
    let hsha = resolve(&cx, &repo, &head).await?;
    let git = &cx.hub.ports.git;
    let cmp = git.compare(&repo, &bsha, &hsha).await?;
    if cx.accepts("diff") || cx.accepts("patch") {
        return Ok(Reply::raw(
            diff_text(&cmp.files),
            "text/plain; charset=utf-8",
        ));
    }
    let ahead = cmp.ahead.len() as u64;
    let status = match (ahead, cmp.behind_by) {
        (0, 0) => "identical",
        (_, 0) => "ahead",
        (0, _) => "behind",
        _ => "diverged",
    };
    let base_commit = git
        .commit(&repo, &bsha)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mb = match &cmp.merge_base {
        Some(m) => git.commit(&repo, m).await?,
        None => None,
    };
    let mut commits = Vec::new();
    for c in &cmp.ahead {
        commits.push(shapes::commit(&cx, &repo, c).await);
    }
    let html = cx.html(&format!("/{}/compare/{base}...{head}", repo.full_name));
    Ok(Reply::json(json!({
        "url": cx.api(&format!("/repos/{}/compare/{base}...{head}", repo.full_name)),
        "html_url": html,
        "permalink_url": cx.html(&format!("/{}/compare/{bsha}...{hsha}", repo.full_name)),
        "diff_url": format!("{html}.diff"),
        "patch_url": format!("{html}.patch"),
        "base_commit": shapes::commit(&cx, &repo, &base_commit).await,
        "merge_base_commit": match &mb { Some(c) => shapes::commit(&cx, &repo, c).await, None => Value::Null },
        "status": status,
        "ahead_by": ahead,
        "behind_by": cmp.behind_by,
        "total_commits": ahead,
        "commits": commits,
        "files": cmp.files.iter().map(|f| file_json(&cx, &repo, &hsha, f)).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn land_blocks_are_replaced_per_branch() {
        let text = "version = 1\n\n[[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n\n[[land]]\nref = \"main\"\nrequired_checks = [\"a\"]\n\n[[land.approvals]]\ncount = 1\n\n[[land]]\nref = \"release/*\"\nrequire_signed = true\n\n[paths]\nsensitive = [\"/x\"]\n";
        let out = strip_land(text, "main");
        assert!(!out.contains("ref = \"main\""));
        assert!(!out.contains("count = 1"));
        assert!(out.contains("ref = \"release/*\""));
        assert!(out.contains("[paths]"));
        let block = land_block(
            "main",
            &json!({"required_status_checks": {"contexts": ["ci"]}, "required_pull_request_reviews": {"required_approving_review_count": 2, "dismiss_stale_reviews": true}, "required_linear_history": true}),
        );
        let full = format!("{out}\n{block}");
        let policy = bee_policy::Policy::parse(&full).expect("valid policy");
        assert!(policy.is_protected("main"));
    }

    #[test]
    fn branch_paths_split() {
        assert_eq!(split_branch("feature/x"), ("feature/x", ""));
        assert_eq!(split_branch("main/protection"), ("main", "protection"));
        assert_eq!(
            split_branch("a/b/protection/required_status_checks"),
            ("a/b", "protection/required_status_checks")
        );
        assert_eq!(split_branch("dev/rename"), ("dev", "rename"));
    }
}
