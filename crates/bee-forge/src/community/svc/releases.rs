//! `ReleaseService`. Reads need `read`; drafts are visible to principals who hold `commit`;
//! creating, changing and deleting releases, assets and tags needs `commit` (a write role),
//! and deleting a release's tag with it additionally needs `manage_refs`.

use bee_policy::{Action, RefAction};
use bee_proto::v1::release_service_server::ReleaseService;
use bee_proto::v1::{
    BeginReleaseAssetUploadRequest, BeginReleaseAssetUploadResponse, CreateReleaseRequest,
    CreateRepoTagRequest, DeleteReleaseAssetRequest, DeleteReleaseAssetResponse,
    DeleteReleaseRequest, DeleteReleaseResponse, DeleteRepoTagRequest, DeleteRepoTagResponse,
    FinalizeReleaseAssetRequest, GenerateReleaseNotesRequest, GenerateReleaseNotesResponse,
    GetLatestReleaseRequest, GetReleaseAssetDownloadUrlRequest, GetReleaseAssetDownloadUrlResponse,
    GetReleaseAssetRequest, GetReleaseAttestationRequest, GetReleaseByTagRequest,
    GetReleaseRequest, GetRepoTagRequest, GetSourceArchiveUrlRequest, GetSourceArchiveUrlResponse,
    KeyValue, LinkReleaseDiscussionRequest, ListReleaseAssetsRequest, ListReleaseAssetsResponse,
    ListReleasesRequest, ListReleasesResponse, ListRepoTagsRequest, ListRepoTagsResponse,
    PublishReleaseRequest, Release, ReleaseAsset, ReleaseAttestation, RepoRole, RepoTag,
    SourceArchiveFormat, UpdateReleaseAssetRequest, UpdateReleaseRequest,
};
use tonic::{Request, Response, Status};

use super::writable;
use crate::community::releases::{NewRelease, ReleasePatch};
use crate::ids::ts;
use crate::service::{ForgeServices, next, page, reply};

fn expr(r: &Option<bee_proto::v1::Revision>) -> String {
    r.as_ref().map(|x| x.expr.clone()).unwrap_or_default()
}

#[tonic::async_trait]
impl ReleaseService for ForgeServices {
    async fn create_release(
        &self,
        req: Request<CreateReleaseRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, Some("release.create"))
                .await?;
            writable(&repo)?;
            if !r.draft {
                self.new_tag_policy(&caller, &repo, &r.tag_name).await?;
            }
            self.community()
                .releases
                .create(
                    &cx,
                    &repo,
                    NewRelease {
                        tag_name: r.tag_name,
                        target: expr(&r.target),
                        name: r.name,
                        body: r.body,
                        draft: r.draft,
                        prerelease: r.prerelease,
                        make_latest: r.make_latest,
                        immutable: r.immutable,
                        generate_notes: r.generate_notes,
                        discussion_category: r.discussion_category_id,
                        previous_tag: r.previous_tag_name,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_release(
        &self,
        req: Request<GetReleaseRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = self.can(&caller, &repo, Action::Commit).await;
            self.community()
                .releases
                .get(&cx, &repo, &r.release_id, drafts)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_release_by_tag(
        &self,
        req: Request<GetReleaseByTagRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = self.can(&caller, &repo, Action::Commit).await;
            self.community()
                .releases
                .get_by_tag(&cx, &repo, &r.tag_name, drafts)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_latest_release(
        &self,
        req: Request<GetLatestReleaseRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            self.community().releases.latest(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_releases(
        &self,
        req: Request<ListReleasesRequest>,
    ) -> Result<Response<ListReleasesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = r.include_drafts && self.can(&caller, &repo, Action::Commit).await;
            let (token, limit) = page(&r.page);
            let (releases, nt) = self
                .community()
                .releases
                .list(&cx, &repo, drafts, r.include_prereleases, &token, limit)
                .await?;
            Ok(ListReleasesResponse {
                releases,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_release(
        &self,
        req: Request<UpdateReleaseRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, Some("release.update"))
                .await?;
            writable(&repo)?;
            if r.draft == Some(false) {
                let tag = match &r.tag_name {
                    Some(t) => t.clone(),
                    None => {
                        self.community()
                            .releases
                            .get(&cx, &repo, &r.release_id, true)
                            .await?
                            .tag_name
                    }
                };
                self.new_tag_policy(&caller, &repo, &tag).await?;
            }
            self.community()
                .releases
                .update(
                    &cx,
                    &repo,
                    &r.release_id,
                    ReleasePatch {
                        tag_name: r.tag_name,
                        target: r.target.map(|t| t.expr),
                        name: r.name,
                        body: r.body,
                        draft: r.draft,
                        prerelease: r.prerelease,
                        make_latest: r.make_latest,
                        immutable: r.immutable,
                        discussion_category: r.discussion_category_id,
                        expected_version: r.expected_version,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_release(
        &self,
        req: Request<DeleteReleaseRequest>,
    ) -> Result<Response<DeleteReleaseResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, Some("release.delete"))
                .await?;
            writable(&repo)?;
            if r.delete_tag {
                self.repo_cx(
                    &caller,
                    &repo,
                    Action::ManageRefs,
                    Some(RepoRole::Maintain),
                    Some("release.delete"),
                )
                .await?;
                let rel = self
                    .community()
                    .releases
                    .get(&cx, &repo, &r.release_id, true)
                    .await?;
                if !rel.draft {
                    self.tag_policy(&caller, &repo, RefAction::Delete, &rel.tag_name)
                        .await?;
                }
            }
            self.community()
                .releases
                .delete(&cx, &repo, &r.release_id, r.delete_tag)
                .await?;
            Ok(DeleteReleaseResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn publish_release(
        &self,
        req: Request<PublishReleaseRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::Commit,
                    None,
                    Some("release.publish"),
                )
                .await?;
            writable(&repo)?;
            let tag = self
                .community()
                .releases
                .get(&cx, &repo, &r.release_id, true)
                .await?
                .tag_name;
            self.new_tag_policy(&caller, &repo, &tag).await?;
            self.community()
                .releases
                .publish(&cx, &repo, &r.release_id, r.make_latest)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn generate_release_notes(
        &self,
        req: Request<GenerateReleaseNotesRequest>,
    ) -> Result<Response<GenerateReleaseNotesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, None)
                .await?;
            let config = String::from_utf8_lossy(&r.config_path).into_owned();
            let n = self
                .community()
                .releases
                .generate_notes(
                    &cx,
                    &repo,
                    &r.tag_name,
                    &expr(&r.target),
                    &r.previous_tag_name,
                    &config,
                )
                .await?;
            Ok(GenerateReleaseNotesResponse {
                name: n.name,
                body: n.body,
                change_requests: n.change_requests,
                previous_tag_name: n.previous_tag_name,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_release_attestation(
        &self,
        req: Request<GetReleaseAttestationRequest>,
    ) -> Result<Response<ReleaseAttestation>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = self.can(&caller, &repo, Action::Commit).await;
            self.community()
                .releases
                .attestation(&cx, &repo, &r.release_id, drafts)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn begin_release_asset_upload(
        &self,
        req: Request<BeginReleaseAssetUploadRequest>,
    ) -> Result<Response<BeginReleaseAssetUploadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::Commit,
                    None,
                    Some("release.asset_upload"),
                )
                .await?;
            writable(&repo)?;
            let t = self
                .community()
                .releases
                .begin_asset(
                    &cx,
                    &repo,
                    &r.release_id,
                    &r.name,
                    &r.label,
                    &r.content_type,
                    r.size,
                )
                .await?;
            Ok(BeginReleaseAssetUploadResponse {
                asset_id: t.asset_id,
                upload_url: t.url.url,
                upload_headers: t
                    .url
                    .headers
                    .into_iter()
                    .map(|(key, value)| KeyValue { key, value })
                    .collect(),
                expires_at: Some(ts(t.url.expires_ms)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn finalize_release_asset(
        &self,
        req: Request<FinalizeReleaseAssetRequest>,
    ) -> Result<Response<ReleaseAsset>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::Commit,
                    None,
                    Some("release.asset_upload"),
                )
                .await?;
            writable(&repo)?;
            self.community()
                .releases
                .finalize_asset(&cx, &repo, &r.asset_id, &r.sha256)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_release_assets(
        &self,
        req: Request<ListReleaseAssetsRequest>,
    ) -> Result<Response<ListReleaseAssetsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = self.can(&caller, &repo, Action::Commit).await;
            let (token, limit) = page(&r.page);
            let (assets, nt) = self
                .community()
                .releases
                .list_assets(&cx, &repo, &r.release_id, drafts, &token, limit)
                .await?;
            Ok(ListReleaseAssetsResponse {
                assets,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_release_asset(
        &self,
        req: Request<GetReleaseAssetRequest>,
    ) -> Result<Response<ReleaseAsset>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = self.can(&caller, &repo, Action::Commit).await;
            self.community()
                .releases
                .get_asset(&cx, &repo, &r.asset_id, drafts)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_release_asset(
        &self,
        req: Request<UpdateReleaseAssetRequest>,
    ) -> Result<Response<ReleaseAsset>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::Commit,
                    None,
                    Some("release.asset_update"),
                )
                .await?;
            writable(&repo)?;
            self.community()
                .releases
                .update_asset(
                    &cx,
                    &repo,
                    &r.asset_id,
                    r.name.as_deref(),
                    r.label.as_deref(),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_release_asset(
        &self,
        req: Request<DeleteReleaseAssetRequest>,
    ) -> Result<Response<DeleteReleaseAssetResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::Commit,
                    None,
                    Some("release.asset_delete"),
                )
                .await?;
            writable(&repo)?;
            self.community()
                .releases
                .delete_asset(&cx, &repo, &r.asset_id)
                .await?;
            Ok(DeleteReleaseAssetResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_release_asset_download_url(
        &self,
        req: Request<GetReleaseAssetDownloadUrlRequest>,
    ) -> Result<Response<GetReleaseAssetDownloadUrlResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let drafts = self.can(&caller, &repo, Action::Commit).await;
            let (url, sha256, size) = self
                .community()
                .releases
                .download_url(&cx, &repo, &r.asset_id, drafts)
                .await?;
            Ok(GetReleaseAssetDownloadUrlResponse {
                url: url.url,
                expires_at: Some(ts(url.expires_ms)),
                sha256,
                size,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_source_archive_url(
        &self,
        req: Request<GetSourceArchiveUrlRequest>,
    ) -> Result<Response<GetSourceArchiveUrlResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            // An archive carries the whole tree: the caller must read every path of it.
            let cx = self
                .fg()
                .authz
                .repo_every_path(&caller, &repo, Action::Read)
                .await?;
            let format = SourceArchiveFormat::try_from(r.format).unwrap_or_default();
            let (url, changeset, tree) = self
                .community()
                .releases
                .archive(&cx, &repo, &expr(&r.at), format, &r.prefix)
                .await?;
            Ok(GetSourceArchiveUrlResponse {
                url: url.url,
                expires_at: Some(ts(url.expires_ms)),
                changeset: Some(changeset),
                tree: Some(tree),
                filtered: false,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_tags(
        &self,
        req: Request<ListRepoTagsRequest>,
    ) -> Result<Response<ListRepoTagsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let (token, limit) = page(&r.page);
            let (tags, nt) = self
                .community()
                .releases
                .list_tags(&cx, &repo, &r.prefix, &token, limit)
                .await?;
            Ok(ListRepoTagsResponse {
                tags,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_repo_tag(
        &self,
        req: Request<GetRepoTagRequest>,
    ) -> Result<Response<RepoTag>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            self.community().releases.get_tag(&cx, &repo, &r.name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_repo_tag(
        &self,
        req: Request<CreateRepoTagRequest>,
    ) -> Result<Response<RepoTag>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, Some("tag.create"))
                .await?;
            writable(&repo)?;
            self.tag_policy(&caller, &repo, RefAction::Create, &r.name)
                .await?;
            self.community()
                .releases
                .create_tag(
                    &cx,
                    &repo,
                    &r.name,
                    &expr(&r.target),
                    &r.message,
                    &r.signature,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_repo_tag(
        &self,
        req: Request<DeleteRepoTagRequest>,
    ) -> Result<Response<DeleteRepoTagResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, Some("tag.delete"))
                .await?;
            writable(&repo)?;
            self.tag_policy(&caller, &repo, RefAction::Delete, &r.name)
                .await?;
            self.community()
                .releases
                .delete_tag(&cx, &repo, &r.name, r.expected_version)
                .await?;
            Ok(DeleteRepoTagResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn link_release_discussion(
        &self,
        req: Request<LinkReleaseDiscussionRequest>,
    ) -> Result<Response<Release>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::Commit,
                    None,
                    Some("release.link_discussion"),
                )
                .await?;
            writable(&repo)?;
            self.community()
                .releases
                .link_discussion(&cx, &repo, &r.release_id, &r.discussion_category_id)
                .await
        }
        .await;
        reply(&caller, out)
    }
}

impl ForgeServices {
    /// Applies the repo policy's ref rules (`[[refs]]`, SPEC §9.4) to `tags/<tag>`: tags made
    /// or removed through releases are governed exactly like tags pushed or created with
    /// `RefService`.
    async fn tag_policy(
        &self,
        caller: &crate::auth::Caller,
        repo: &bee_proto::v1::Repo,
        action: RefAction,
        tag: &str,
    ) -> crate::error::Result<()> {
        let Some(src) = &self.fg().hooks.policy else {
            return Ok(());
        };
        let Some(policy) = src.policy(repo).await? else {
            return Ok(());
        };
        // Forge repo roles reach the policy as `@repo-role/<role>` (SPEC §9.4).
        let actors = self.fg().authz.policy_actors(caller, repo).await?;
        let name = format!("tags/{tag}");
        let d = policy.check_ref_chain(&actors, action, &name);
        if d.allowed {
            return Ok(());
        }
        Err(crate::error::ForgeError::policy_denied(
            &d.rule,
            vec![format!("ref:{name}")],
            d.detail,
        ))
    }

    /// [`Self::tag_policy`] for a tag that publishing would create (only when it is absent).
    async fn new_tag_policy(
        &self,
        caller: &crate::auth::Caller,
        repo: &bee_proto::v1::Repo,
        tag: &str,
    ) -> crate::error::Result<()> {
        if tag.is_empty()
            || self
                .community()
                .ports
                .tags
                .get(&repo.id, tag)
                .await?
                .is_some()
        {
            return Ok(());
        }
        self.tag_policy(caller, repo, RefAction::Create, tag).await
    }
}
