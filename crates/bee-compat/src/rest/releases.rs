//! Releases and assets. Asset bytes arrive on the uploads path (`upload_url`) and go to the
//! signed URL the forge issues, through the [`crate::ports::AssetStore`].

use std::sync::Arc;

use axum::extract::Request;
use axum::response::Response;
use bee_proto::v1::release_service_server::ReleaseService as _;
use bee_proto::v1::{
    BeginReleaseAssetUploadRequest, CreateReleaseRequest, DeleteReleaseAssetRequest,
    DeleteReleaseRequest, FinalizeReleaseAssetRequest, GenerateReleaseNotesRequest,
    GetLatestReleaseRequest, GetReleaseAssetDownloadUrlRequest, GetReleaseAssetRequest,
    GetReleaseByTagRequest, GetReleaseRequest, ListReleaseAssetsRequest, ListReleasesRequest,
    Principal, Release, ReleaseAsset, ReleaseAssetState, ReleaseMakeLatest, Repo, Revision,
    UpdateReleaseAssetRequest, UpdateReleaseRequest,
};
use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{b, need, repo_named, repo_of, rid, s};
use crate::error::{ApiError, ApiResult};
use crate::http::{
    Ctx, Params, Reply, all_pages, encode, error_reply, finish, rate_error, take_rate,
};
use crate::hub::Compat;
use crate::shapes;
use crate::time::{iso, iso_or};

async fn asset_json(cx: &Ctx, repo: &Repo, tag: &str, a: &ReleaseAsset) -> Value {
    let id = cx.hub.ids.remember("asset", &a.id, &repo.id);
    let digest: String = a.sha256.iter().map(|b| format!("{b:02x}")).collect();
    json!({
        "url": cx.api(&format!("/repos/{}/releases/assets/{id}", repo.full_name)),
        "browser_download_url": cx.html(&format!("/{}/releases/download/{}/{}", repo.full_name, encode(tag), encode(&a.name))),
        "id": id,
        "node_id": crate::ids::node("ReleaseAsset", &a.id),
        "name": a.name,
        "label": if a.label.is_empty() { Value::Null } else { json!(a.label) },
        "state": if ReleaseAssetState::try_from(a.state).unwrap_or_default() == ReleaseAssetState::Uploaded { "uploaded" } else { "open" },
        "content_type": a.content_type,
        "size": a.size,
        "digest": if digest.is_empty() { Value::Null } else { json!(format!("sha256:{digest}")) },
        "download_count": a.download_count,
        "created_at": iso(&a.created_at),
        "updated_at": iso_or(&a.updated_at, &a.created_at),
        "uploader": shapes::user(cx, a.uploader.as_ref().unwrap_or(&Principal::default())).await,
    })
}

/// GitHub's release object.
pub(crate) async fn release_json(cx: &Ctx, repo: &Repo, r: &Release) -> Value {
    let id = cx.hub.ids.remember("release", &r.id, &repo.id);
    let base = cx.api(&format!("/repos/{}/releases/{id}", repo.full_name));
    let mut assets = Vec::new();
    for a in &r.assets {
        assets.push(asset_json(cx, repo, &r.tag_name, a).await);
    }
    let commitish = if r.target_ref.is_empty() {
        match &r.target {
            Some(t) => cx
                .hub
                .ports
                .git
                .git_id(repo, t)
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
            None => String::new(),
        }
    } else {
        r.target_ref.clone()
    };
    json!({
        "url": base,
        "html_url": cx.html(&format!("/{}/releases/tag/{}", repo.full_name, encode(&r.tag_name))),
        "assets_url": format!("{base}/assets"),
        "upload_url": format!("{}{{?name,label}}", cx.uploads(&format!("/repos/{}/releases/{id}/assets", repo.full_name))),
        "tarball_url": cx.api(&format!("/repos/{}/tarball/{}", repo.full_name, encode(&r.tag_name))),
        "zipball_url": cx.api(&format!("/repos/{}/zipball/{}", repo.full_name, encode(&r.tag_name))),
        "discussion_url": Value::Null,
        "id": id,
        "node_id": crate::ids::node("Release", &r.id),
        "tag_name": r.tag_name,
        "target_commitish": commitish,
        "name": if r.name.is_empty() { Value::Null } else { json!(r.name) },
        "body": if r.body.is_empty() { Value::Null } else { json!(r.body) },
        "draft": r.draft,
        "prerelease": r.prerelease,
        "immutable": r.immutable,
        "make_latest": if r.latest { "true" } else { "false" },
        "created_at": iso(&r.created_at),
        "published_at": iso(&r.published_at),
        "updated_at": iso_or(&r.updated_at, &r.created_at),
        "author": shapes::user(cx, r.author.as_ref().unwrap_or(&Principal::default())).await,
        "assets": assets,
    })
}

async fn all_releases(cx: &Ctx, repo: &Repo) -> ApiResult<Vec<Release>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListReleasesRequest {
            repo: rid(&repo.id),
            include_drafts: true,
            include_prereleases: true,
            page: Some(page),
        });
        async move {
            let r = svc.list_releases(req).await?.into_inner();
            Ok((r.releases, r.page))
        }
    })
    .await
}

async fn release_by_num(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<Release> {
    if let Some(ix) = cx.hub.ids.get("release", n)
        && let Ok(r) = cx
            .hub
            .svc
            .get_release(cx.req(GetReleaseRequest {
                repo: rid(&repo.id),
                release_id: ix.id.clone(),
                include_html: false,
            }))
            .await
    {
        return Ok(r.into_inner());
    }
    all_releases(cx, repo)
        .await?
        .into_iter()
        .find(|r| crate::ids::num("release", &r.id) == n)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/releases`.
pub(crate) async fn list(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut rels = all_releases(&cx, &repo).await?;
    rels.sort_by(|a, b| {
        let t = |r: &Release| {
            r.created_at
                .as_ref()
                .map(|t| (t.seconds, t.nanos))
                .unwrap_or((0, 0))
        };
        t(b).cmp(&t(a)).then(b.id.cmp(&a.id))
    });
    let total = rels.len();
    let (page, per) = cx.page();
    let mut items = Vec::new();
    for r in rels.iter().skip((page - 1) * per).take(per) {
        items.push(release_json(&cx, &repo, r).await);
    }
    Ok(cx.paged(items, total))
}

/// The forge revision for a GitHub `target_commitish` (branch, tag or sha).
async fn revision_of(cx: &Ctx, repo: &Repo, commitish: &str) -> ApiResult<Option<Revision>> {
    if commitish.is_empty() {
        return Ok(None);
    }
    let sha = super::git::resolve(cx, repo, commitish)
        .await
        .map_err(|_| ApiError::validation("Release", "target_commitish", "invalid", commitish))?;
    let cs = cx
        .hub
        .ports
        .git
        .changeset(repo, &sha)
        .await?
        .ok_or_else(|| ApiError::validation("Release", "target_commitish", "invalid", commitish))?;
    Ok(Some(Revision {
        expr: super::revision_text(&cs)?,
    }))
}

fn make_latest(v: &Value) -> ReleaseMakeLatest {
    match v.get("make_latest") {
        Some(Value::String(s)) if s == "true" => ReleaseMakeLatest::True,
        Some(Value::String(s)) if s == "false" => ReleaseMakeLatest::False,
        Some(Value::Bool(true)) => ReleaseMakeLatest::True,
        Some(Value::Bool(false)) => ReleaseMakeLatest::False,
        _ => ReleaseMakeLatest::Legacy,
    }
}

/// `POST /repos/{owner}/{repo}/releases`.
pub(crate) async fn create(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let tag = need(&body, "tag_name", "Release")?.to_string();
    let default = if repo.default_ref.is_empty() {
        "main".to_string()
    } else {
        repo.default_ref.clone()
    };
    let target = revision_of(&cx, &repo, s(&body, "target_commitish").unwrap_or(&default)).await?;
    let r = cx
        .hub
        .svc
        .create_release(
            cx.req(CreateReleaseRequest {
                repo: rid(&repo.id),
                tag_name: tag,
                target,
                name: s(&body, "name").unwrap_or("").to_string(),
                body: s(&body, "body").unwrap_or("").to_string(),
                draft: b(&body, "draft").unwrap_or(false),
                prerelease: b(&body, "prerelease").unwrap_or(false),
                make_latest: make_latest(&body) as i32,
                immutable: false,
                generate_notes: b(&body, "generate_release_notes").unwrap_or(false),
                discussion_category_id: s(&body, "discussion_category_name")
                    .unwrap_or("")
                    .to_string(),
                previous_tag_name: String::new(),
            }),
        )
        .await?
        .into_inner();
    let v = release_json(&cx, &repo, &r).await;
    let loc = v["url"].as_str().unwrap_or("").to_string();
    Ok(Reply::created(v).header("location", loc))
}

/// `GET /repos/{owner}/{repo}/releases/latest`.
pub(crate) async fn latest(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = cx
        .hub
        .svc
        .get_latest_release(cx.req(GetLatestReleaseRequest {
            repo: rid(&repo.id),
            include_html: false,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(release_json(&cx, &repo, &r).await))
}

/// `GET /repos/{owner}/{repo}/releases/tags/{tag}`.
pub(crate) async fn by_tag(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = cx
        .hub
        .svc
        .get_release_by_tag(cx.req(GetReleaseByTagRequest {
            repo: rid(&repo.id),
            tag_name: p.get("tag").to_string(),
            include_html: false,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(release_json(&cx, &repo, &r).await))
}

/// `GET /repos/{owner}/{repo}/releases/{id}`.
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = release_by_num(&cx, &repo, p.num("id")?).await?;
    Ok(Reply::json(release_json(&cx, &repo, &r).await))
}

/// `PATCH /repos/{owner}/{repo}/releases/{id}`.
pub(crate) async fn update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = release_by_num(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    let target = match s(&body, "target_commitish") {
        Some(t) => revision_of(&cx, &repo, t).await?,
        None => None,
    };
    let r = cx
        .hub
        .svc
        .update_release(cx.req(UpdateReleaseRequest {
            repo: rid(&repo.id),
            release_id: r.id.clone(),
            expected_version: 0,
            tag_name: s(&body, "tag_name").map(str::to_string),
            target,
            name: s(&body, "name").map(str::to_string),
            body: s(&body, "body").map(str::to_string),
            draft: b(&body, "draft"),
            prerelease: b(&body, "prerelease"),
            make_latest: body.get("make_latest").map(|_| make_latest(&body) as i32),
            immutable: None,
            discussion_category_id: None,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(release_json(&cx, &repo, &r).await))
}

/// `DELETE /repos/{owner}/{repo}/releases/{id}` (the tag stays, as on GitHub).
pub(crate) async fn delete(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = release_by_num(&cx, &repo, p.num("id")?).await?;
    cx.hub
        .svc
        .delete_release(cx.req(DeleteReleaseRequest {
            repo: rid(&repo.id),
            release_id: r.id,
            delete_tag: false,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `POST /repos/{owner}/{repo}/releases/generate-notes`.
pub(crate) async fn generate_notes(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let target = match s(&body, "target_commitish") {
        Some(t) => revision_of(&cx, &repo, t).await?,
        None => None,
    };
    let r = cx
        .hub
        .svc
        .generate_release_notes(
            cx.req(GenerateReleaseNotesRequest {
                repo: rid(&repo.id),
                tag_name: need(&body, "tag_name", "Release")?.to_string(),
                target,
                previous_tag_name: s(&body, "previous_tag_name").unwrap_or("").to_string(),
                config_path: s(&body, "configuration_file_path")
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec(),
            }),
        )
        .await?
        .into_inner();
    Ok(Reply::json(json!({"name": r.name, "body": r.body})))
}

/// `GET /repos/{owner}/{repo}/releases/{id}/assets`.
pub(crate) async fn assets(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = release_by_num(&cx, &repo, p.num("id")?).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListReleaseAssetsRequest {
            repo: rid(&repo.id),
            release_id: r.id.clone(),
            page: Some(page),
        });
        async move {
            let x = svc.list_release_assets(req).await?.into_inner();
            Ok((x.assets, x.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for a in &list {
        items.push(asset_json(&cx, &repo, &r.tag_name, a).await);
    }
    Ok(cx.paginate(items))
}

async fn asset_by_num(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<(ReleaseAsset, String)> {
    if let Some(ix) = cx.hub.ids.get("asset", n)
        && let Ok(a) = cx
            .hub
            .svc
            .get_release_asset(cx.req(GetReleaseAssetRequest {
                repo: rid(&repo.id),
                asset_id: ix.id.clone(),
            }))
            .await
    {
        let a = a.into_inner();
        let tag = all_releases(cx, repo)
            .await?
            .into_iter()
            .find(|r| r.id == a.release_id)
            .map(|r| r.tag_name)
            .unwrap_or_default();
        return Ok((a, tag));
    }
    for r in all_releases(cx, repo).await? {
        if let Some(a) = r
            .assets
            .iter()
            .find(|a| crate::ids::num("asset", &a.id) == n)
        {
            return Ok((a.clone(), r.tag_name.clone()));
        }
    }
    Err(ApiError::not_found())
}

/// `GET /repos/{owner}/{repo}/releases/assets/{id}`; with `Accept: application/octet-stream`
/// a redirect to the signed download URL.
pub(crate) async fn asset(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (a, tag) = asset_by_num(&cx, &repo, p.num("id")?).await?;
    if cx
        .header("accept")
        .is_some_and(|h| h.contains("application/octet-stream"))
    {
        let u = cx
            .hub
            .svc
            .get_release_asset_download_url(cx.req(GetReleaseAssetDownloadUrlRequest {
                repo: rid(&repo.id),
                asset_id: a.id.clone(),
            }))
            .await?
            .into_inner();
        return Ok(Reply::redirect(&u.url));
    }
    Ok(Reply::json(asset_json(&cx, &repo, &tag, &a).await))
}

/// `PATCH /repos/{owner}/{repo}/releases/assets/{id}`.
pub(crate) async fn update_asset(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (a, tag) = asset_by_num(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    let a = cx
        .hub
        .svc
        .update_release_asset(cx.req(UpdateReleaseAssetRequest {
            repo: rid(&repo.id),
            asset_id: a.id.clone(),
            name: s(&body, "name").map(str::to_string),
            label: s(&body, "label").map(str::to_string),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(asset_json(&cx, &repo, &tag, &a).await))
}

/// `DELETE /repos/{owner}/{repo}/releases/assets/{id}`.
pub(crate) async fn delete_asset(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (a, _) = asset_by_num(&cx, &repo, p.num("id")?).await?;
    cx.hub
        .svc
        .delete_release_asset(cx.req(DeleteReleaseAssetRequest {
            repo: rid(&repo.id),
            asset_id: a.id,
        }))
        .await?;
    Ok(Reply::no_content())
}

async fn do_upload(cx: &Ctx) -> ApiResult<Reply> {
    let parts: Vec<&str> = cx.path.split('/').filter(|s| !s.is_empty()).collect();
    let ["repos", owner, name, "releases", id, "assets"] = parts.as_slice() else {
        return Err(ApiError::not_found());
    };
    if cx.method != http::Method::POST {
        return Err(ApiError::not_found());
    }
    let repo = repo_named(cx, owner, name).await?;
    let n: u64 = id.parse().map_err(|_| ApiError::not_found())?;
    let rel = release_by_num(cx, &repo, n).await?;
    let file = cx
        .q("name")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::missing_field("ReleaseAsset", "name"))?
        .to_string();
    if rel.assets.iter().any(|a| a.name == file) {
        return Err(ApiError::validation(
            "ReleaseAsset",
            "name",
            "already_exists",
            "",
        ));
    }
    let ctype = cx
        .header("content-type")
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes: Bytes = cx.body.clone();
    let begin = cx
        .hub
        .svc
        .begin_release_asset_upload(cx.req(BeginReleaseAssetUploadRequest {
            repo: rid(&repo.id),
            release_id: rel.id.clone(),
            name: file,
            label: cx.q("label").unwrap_or("").to_string(),
            content_type: ctype,
            size: bytes.len() as u64,
        }))
        .await?
        .into_inner();
    let headers: Vec<(String, String)> = begin
        .upload_headers
        .iter()
        .map(|kv| (kv.key.clone(), kv.value.clone()))
        .collect();
    let digest = Sha256::digest(&bytes).to_vec();
    cx.hub
        .ports
        .assets
        .put(&begin.upload_url, &headers, bytes)
        .await?;
    let a = cx
        .hub
        .svc
        .finalize_release_asset(cx.req(FinalizeReleaseAssetRequest {
            repo: rid(&repo.id),
            asset_id: begin.asset_id,
            sha256: digest,
        }))
        .await?
        .into_inner();
    Ok(Reply::created(
        asset_json(cx, &repo, &rel.tag_name, &a).await,
    ))
}

/// `POST /api/uploads/repos/{owner}/{repo}/releases/{id}/assets?name=&label=`.
pub(crate) async fn upload(hub: Arc<Compat>, req: Request) -> Response {
    let cx = match crate::http::context(hub, "/api/uploads", req).await {
        Ok(cx) => cx,
        Err(reply) => return finish(None, reply, None),
    };
    let state = match take_rate(&cx, "core") {
        Ok(s) => s,
        Err(s) => {
            return finish(
                Some(&cx),
                error_reply(&rate_error(&cx, &s)),
                Some((s, "core")),
            );
        }
    };
    let reply = match do_upload(&cx).await {
        Ok(r) => r,
        Err(e) => error_reply(&e),
    };
    finish(Some(&cx), reply, Some((state, "core")))
}
