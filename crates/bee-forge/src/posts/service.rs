//! The tonic `PostService` over [`Posts`], following the forge service shape
//! (`docs/spec/forge-core.md` §7), and the post as each caller sees it ([`View`]).
//!
//! | RPCs | who |
//! |---|---|
//! | `GetPost`, `ListRepoPosts`, `GetChangePost` | repo `read` (signed-out callers on public repos too) |
//! | `CreatePost`, media upload without a change request | `commit` on the repo |
//! | `SetChangePost`, `ClearChangePost`, media upload | whoever may edit the change request: its author (with `read`) or `commit` on the repo |
//! | `ApprovePost` | a human with `commit` who is not the post's author |
//! | `WithdrawPost` | the post's author or a repo admin |
//! | `LikePost`, `RepostPost` and their undo, `ListFollowingFeed` | an account (agents too): a token that allows `commit` (feed: `read`), and repo `read` |
//! | `ListPostAdoptions` | an account that reads the post's repo; only adoptions in repos the caller reads are listed |
//!
//! Every post a call returns is checked for repo `read`; one in a deleted repo is
//! `NOT_FOUND`. `PostCounts.adoptions` counts adoptions in repos the caller reads, and the
//! `inspired_by` of an issue or change request is set only when the caller reads the post's
//! repo ([`fill_change_posts`], [`fill_issue_inspirations`]). `Post.attachments` are cards
//! read from the attachments as they are now ([`super::attach`]). Media URLs are signed for
//! the caller's principal chain (empty for a signed-out caller) and expire after 15 minutes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bee_policy::Action;
use bee_proto::v1::post_service_server::{PostService, PostServiceServer};
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::adopt::{self, AdoptionRec};
use super::feed::{self, Dir, Readable, Sources};
use super::model::PostRec;
use super::store::{Content, Posts, is_own_media_owner, own_media_owner};
use super::{MEDIA_PREFIX, store::At};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::media::ChainLink;
use crate::service::{ForgeServices, next, page, reply};

type Res<T> = std::result::Result<Response<T>, Status>;

/// `PostService` over a forge and its posts module.
#[derive(Clone)]
pub struct PostServices {
    base: ForgeServices,
    posts: Arc<Posts>,
}

/// What the caller may do in one repo.
#[derive(Clone)]
struct Seen {
    cx: Cx,
    commit: bool,
    admin: bool,
    full_name: String,
}

/// Posts as one caller sees them: visibility, counts, what they did and may do, media URLs.
/// Repo decisions are memoized for the call.
pub(crate) struct View<'a> {
    forge: &'a Forge,
    posts: &'a Posts,
    caller: &'a Caller,
    chain: Vec<ChainLink>,
    repos: BTreeMap<String, Option<Seen>>,
}

impl<'a> View<'a> {
    pub(crate) fn new(forge: &'a Forge, posts: &'a Posts, caller: &'a Caller) -> View<'a> {
        let chain = if caller.is_anonymous() {
            Vec::new()
        } else {
            caller
                .token
                .chain()
                .into_iter()
                .map(|p| (p.id, p.kind.as_str().to_string()))
                .collect()
        };
        View {
            forge,
            posts,
            caller,
            chain,
            repos: BTreeMap::new(),
        }
    }

    /// The caller's view of a live repo it may read, else `None`.
    async fn seen(&mut self, repo_id: &str) -> Option<Seen> {
        if let Some(v) = self.repos.get(repo_id) {
            return v.clone();
        }
        let authz = &self.forge.authz;
        let r = RepoRef {
            repo: Some(repo_ref::Repo::Id(repo_id.to_string())),
        };
        let v = match self.forge.orgs.resolve_repo(&self.caller.cx, &r).await {
            Ok(repo) => match authz
                .repo(self.caller, &repo, Action::Read, &[], None, None)
                .await
            {
                Ok(cx) => {
                    let commit = !self.caller.is_anonymous()
                        && authz
                            .repo(self.caller, &repo, Action::Commit, &[], None, None)
                            .await
                            .is_ok();
                    let admin = commit
                        && authz
                            .repo(
                                self.caller,
                                &repo,
                                Action::Commit,
                                &[],
                                None,
                                Some(RepoRole::Admin),
                            )
                            .await
                            .is_ok();
                    Some(Seen {
                        cx,
                        commit,
                        admin,
                        full_name: repo.full_name.clone(),
                    })
                }
                Err(_) => None,
            },
            Err(_) => None,
        };
        self.repos.insert(repo_id.to_string(), v.clone());
        v
    }

    /// The post as the caller sees it (the caller reads its repo).
    pub(crate) async fn post(&mut self, rec: &PostRec, version: u64) -> Result<Post> {
        let repo_id = crate::community::common::ref_repo_id(&rec.post().repo);
        let seen = self
            .seen(&repo_id)
            .await
            .ok_or_else(|| ForgeError::not_found(format!("post {}", rec.post().id)))?;
        let me = &self.caller.cx.actor_id;
        let e = self.posts.engagement(&seen.cx, rec, me).await?;
        let adoptions = self
            .readable_adoptions(&seen.cx, &rec.post().id)
            .await?
            .len() as u64;
        let attachments = self.posts.attachments(&seen.cx, rec.post()).await?;
        let mut p = rec.post().clone();
        p.version = version;
        p.attachments = attachments;
        // A `RepoRef` carries one of id or name; clients (agents too) read the name.
        p.repo = Some(RepoRef {
            repo: Some(repo_ref::Repo::FullName(seen.full_name.clone())),
        });
        let state = rec.state();
        let open = matches!(state, PostState::Draft | PostState::PendingApproval);
        let human = self.caller.cx.actor_kind == PrincipalKind::Human;
        let signed_in = !self.caller.is_anonymous();
        p.counts = Some(PostCounts {
            likes: e.likes,
            reposts: e.reposts,
            comments: e.comments,
            adoptions,
        });
        p.viewer = Some(PostViewer {
            liked: e.liked,
            reposted: e.reposted,
            // A post of its own is not edited (it is withdrawn and written again).
            can_edit: signed_in
                && open
                && !p.change_id.is_empty()
                && (seen.commit || e.change_author == *me),
            can_approve: open
                && human
                && seen.commit
                && !rec.by_human()
                && p.approved_by.is_none()
                && rec.author_id() != me,
            can_withdraw: signed_in
                && matches!(state, PostState::PendingApproval | PostState::Published)
                && (rec.author_id() == me || seen.admin),
        });
        if state == PostState::Withdrawn {
            p.text.clear();
            p.media.clear();
            p.body_markdown.clear();
            p.article = None;
        } else {
            let now = self.posts.db.now_ms();
            for m in &mut p.media {
                let key = format!("{MEDIA_PREFIX}/{repo_id}/{}", m.id);
                m.url = self
                    .posts
                    .uploads
                    .signer()
                    .download_url(&key, &self.chain, now)
                    .url;
            }
        }
        Ok(p)
    }

    /// A post's adoptions in repos the caller reads.
    pub(crate) async fn readable_adoptions(
        &mut self,
        cx: &Cx,
        post_id: &str,
    ) -> Result<Vec<AdoptionRec>> {
        let mut out = Vec::new();
        for a in self.posts.adoptions(cx, post_id).await? {
            if self.seen(&a.repo_id).await.is_some() {
                out.push(a);
            }
        }
        Ok(out)
    }

    /// The `inspired_by` of a subject for the caller: its post, when the caller reads the
    /// post's repo.
    pub(crate) async fn inspired(&mut self, a: &AdoptionRec) -> Option<PostRef> {
        let seen = self.seen(&a.source_repo_id()).await?;
        let mut p = a.post.clone()?;
        p.repo = Some(RepoRef {
            repo: Some(repo_ref::Repo::FullName(seen.full_name)),
        });
        Some(p)
    }

    /// An adoption as the caller sees it (its repo is one the caller reads).
    async fn adoption(&mut self, a: &AdoptionRec) -> PostAdoption {
        let name = self
            .seen(&a.repo_id)
            .await
            .map(|s| s.full_name)
            .unwrap_or_default();
        let repo = Some(RepoRef {
            repo: Some(repo_ref::Repo::FullName(name)),
        });
        PostAdoption {
            repo: repo.clone(),
            subject: Some(SubjectRef {
                kind: a.kind,
                repo,
                id: a.subject_id.clone(),
            }),
            title: a.title.clone(),
            author: a.author.clone(),
            created_at: Some(ts(a.created_ms)),
        }
    }

    /// The URL of finalized media for the caller.
    fn media_url(&self, rec: &crate::media::MediaRec) -> String {
        self.posts
            .uploads
            .download_url(rec, &self.chain, self.posts.db.now_ms())
    }
}

#[async_trait]
impl Readable for View<'_> {
    async fn reads(&mut self, repo_id: &str) -> bool {
        self.seen(repo_id).await.is_some()
    }
}

/// Fills `ChangeRequest.post` and `ChangeRequest.inspired_by` for the caller (the change
/// requests are in `repo`, which the caller reads). A no-op when the posts module is not
/// installed.
pub(crate) async fn fill_change_posts(
    forge: &Forge,
    posts: Option<&Arc<Posts>>,
    caller: &Caller,
    cx: &Cx,
    repo: &Repo,
    changes: &mut [ChangeRequest],
) -> Result<()> {
    let Some(posts) = posts else {
        return Ok(());
    };
    let ids: Vec<String> = changes.iter().map(|c| c.id.clone()).collect();
    let found = posts.for_changes(cx, &repo.id, &ids).await?;
    let inspired = posts.change_inspirations(cx, &ids).await?;
    if found.is_empty() && inspired.is_empty() {
        return Ok(());
    }
    let mut view = View::new(forge, posts, caller);
    for c in changes.iter_mut() {
        if let Some((rec, v)) = found.get(&c.id) {
            c.post = Some(Box::new(view.post(rec, *v).await?));
        }
        c.inspired_by = match inspired.get(&c.id) {
            Some(a) => view.inspired(a).await,
            None => None,
        };
    }
    Ok(())
}

/// Sets `Issue.inspired_by` for the caller where the source post's repo is readable (and
/// clears it elsewhere). A no-op when the posts module is not installed.
pub(crate) async fn fill_issue_inspirations(
    forge: &Forge,
    caller: &Caller,
    cx: &Cx,
    issues: &mut [Issue],
) -> Result<()> {
    let Some(posts) = forge.work.posts() else {
        return Ok(());
    };
    let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
    let inspired = posts.issue_inspirations(cx, &ids).await?;
    if inspired.is_empty() {
        return Ok(());
    }
    let mut view = View::new(forge, posts, caller);
    for i in issues.iter_mut() {
        i.inspired_by = match inspired.get(&i.id) {
            Some(a) => view.inspired(a).await,
            None => None,
        };
    }
    Ok(())
}

fn media_view(rec: &crate::media::MediaRec, url: String) -> PostMedia {
    PostMedia {
        id: rec.id.clone(),
        kind: if rec.class == crate::media::VIDEO.name {
            PostMediaKind::Video as i32
        } else {
            PostMediaKind::Image as i32
        },
        content_type: if rec.content_type.is_empty() {
            rec.declared_type.clone()
        } else {
            rec.content_type.clone()
        },
        size_bytes: if rec.size == 0 {
            rec.declared_size
        } else {
            rec.size
        },
        width: rec.width,
        height: rec.height,
        duration_ms: rec.duration_ms,
        alt_text: rec.alt_text.clone(),
        sha256: rec.sha256.clone(),
        url,
    }
}

impl PostServices {
    /// The service over `forge` and `posts`.
    pub fn new(forge: Arc<Forge>, posts: Arc<Posts>) -> PostServices {
        PostServices {
            base: ForgeServices::new(forge),
            posts,
        }
    }

    /// The tonic server.
    pub fn post_service(&self) -> PostServiceServer<PostServices> {
        PostServiceServer::new(self.clone())
    }

    /// Adds `PostService` to a tonic router builder.
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.post_service());
    }

    fn forge(&self) -> &Arc<Forge> {
        self.base.forge()
    }

    async fn read_cx(&self, caller: &Caller, repo: &Repo) -> Result<Cx> {
        self.forge()
            .authz
            .repo(caller, repo, Action::Read, &[], None, None)
            .await
    }

    /// The right to write a change request's post: its author (reading the repo) or `commit`
    /// on the repo; never on an archived repo.
    async fn edit_cx(
        &self,
        caller: &Caller,
        repo: &Repo,
        change_id: &str,
        audit: &str,
    ) -> Result<Cx> {
        let read = self.read_cx(caller, repo).await;
        let read = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, read)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        if !caller.token.allows_action(Action::Commit) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `commit`",
            ));
        }
        let author = self.posts.change_author(&read, repo, change_id).await?;
        if author == caller.cx.actor_id {
            return Ok(read);
        }
        let write = self
            .forge()
            .authz
            .repo(caller, repo, Action::Commit, &[], None, None)
            .await;
        self.base
            .guarded(caller, &repo.org_id, &repo.id, audit, write)
            .await
    }

    /// The right to write a post of its own in `repo`: `commit` on the repo, with a token that
    /// allows it; never on an archived repo. As for a change request's post, a caller who
    /// cannot read the repo learns nothing about it (`NOT_FOUND`).
    async fn write_cx(&self, caller: &Caller, repo: &Repo, audit: &str) -> Result<Cx> {
        let read = self.read_cx(caller, repo).await;
        self.base
            .guarded(caller, &repo.org_id, &repo.id, audit, read)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        if !caller.token.allows_action(Action::Commit) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `commit`",
            ));
        }
        let write = self
            .forge()
            .authz
            .repo(caller, repo, Action::Commit, &[], None, None)
            .await;
        self.base
            .guarded(caller, &repo.org_id, &repo.id, audit, write)
            .await
    }

    /// The right to upload or finalize media owned by `owner`: a change request (whoever may
    /// edit it), or the uploader of media for a post of its own (that principal, with
    /// `commit`; to anyone else the media is `NOT_FOUND`).
    async fn media_cx(
        &self,
        caller: &Caller,
        repo: &Repo,
        owner: &str,
        media_id: &str,
    ) -> Result<Cx> {
        if !is_own_media_owner(owner) {
            return self.edit_cx(caller, repo, owner, "post.media_upload").await;
        }
        let cx = self.write_cx(caller, repo, "post.media_upload").await?;
        if owner != own_media_owner(&caller.cx.actor_id) {
            return Err(ForgeError::not_found(format!("post media {media_id}")));
        }
        Ok(cx)
    }

    /// An account-level write (likes, reposts): the token allows `commit`.
    fn account_cx(&self, caller: &Caller) -> Result<Cx> {
        if caller.is_anonymous() {
            return Err(ForgeError::new(
                ErrorReason::Unauthenticated,
                "sign in to like, repost or read your feed",
            ));
        }
        self.base.user_cx(caller, Action::Commit)
    }

    /// The post `post_id` in `repo` (when given, it must be the post's repo) as the caller
    /// sees it, with the record.
    async fn located(
        &self,
        caller: &Caller,
        repo: &Option<RepoRef>,
        post_id: &str,
    ) -> Result<(Repo, Cx, PostRec, u64)> {
        let repo_id = self.posts.locate(&caller.cx, post_id).await?;
        let r = RepoRef {
            repo: Some(repo_ref::Repo::Id(repo_id.clone())),
        };
        let found = self.forge().orgs.resolve_repo(&caller.cx, &r).await;
        let missing = || ForgeError::not_found(format!("post {post_id}"));
        let target = found.map_err(|_| missing())?;
        if repo.as_ref().is_some_and(|r| r.repo.is_some()) {
            let named = self.base.repo(caller, repo).await.map_err(|_| missing())?;
            if named.id != target.id {
                return Err(missing());
            }
        }
        let cx = self.read_cx(caller, &target).await.map_err(|e| {
            if e.reason == ErrorReason::NotFound {
                missing()
            } else {
                e
            }
        })?;
        let (rec, v) = self.posts.get(&cx, &target.id, post_id).await?;
        Ok((target, cx, rec, v))
    }

    async fn serve(&self, caller: &Caller, rec: &PostRec, version: u64) -> Result<Post> {
        View::new(self.forge(), &self.posts, caller)
            .post(rec, version)
            .await
    }

    async fn engage(&self, caller: &Caller, post_id: &str, on: bool, repost: bool) -> Result<Post> {
        let cx = self.account_cx(caller)?;
        let (repo, _, _, _) = self.located(caller, &None, post_id).await?;
        let mut cx = cx;
        cx.decision = format!("{} repo:read", cx.decision);
        if repost {
            self.posts.repost(&cx, &repo.id, post_id, on).await?;
        } else {
            self.posts.like(&cx, &repo.id, post_id, on).await?;
        }
        let (rec, v) = self.posts.get(&cx, &repo.id, post_id).await?;
        self.serve(caller, &rec, v).await
    }
}

#[async_trait]
impl PostService for PostServices {
    async fn create_post(&self, req: Request<CreatePostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.write_cx(&caller, &repo, "post.create").await?;
            let content = Content {
                body_markdown: r.body_markdown.clone(),
                article: r.article.clone(),
                attachments: r.attachments.clone(),
            };
            let (rec, v) = self
                .posts
                .create(&cx, &repo, &r.text, &r.media_ids, &content)
                .await?;
            self.serve(&caller, &rec, v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_change_post(&self, req: Request<SetChangePostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .edit_cx(&caller, &repo, &r.change_id, "post.set")
                .await?;
            let content = Content {
                body_markdown: r.body_markdown.clone(),
                article: r.article.clone(),
                attachments: r.attachments.clone(),
            };
            let (rec, v) = self
                .posts
                .set_with(
                    &cx,
                    &repo,
                    &r.change_id,
                    &r.text,
                    &r.media_ids,
                    &content,
                    r.expected_version,
                )
                .await?;
            self.serve(&caller, &rec, v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn clear_change_post(
        &self,
        req: Request<ClearChangePostRequest>,
    ) -> Res<ClearChangePostResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .edit_cx(&caller, &repo, &r.change_id, "post.clear")
                .await?;
            self.posts.clear(&cx, &repo, &r.change_id).await?;
            Ok(ClearChangePostResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_change_post(&self, req: Request<GetChangePostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            if r.change_id.is_empty() {
                return Err(ForgeError::missing("change_id"));
            }
            let (rec, v) = self
                .posts
                .for_change(&cx, &repo.id, &r.change_id)
                .await?
                .ok_or_else(|| {
                    ForgeError::not_found(format!("a post on change request {}", r.change_id))
                })?;
            self.serve(&caller, &rec, v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn begin_post_media_upload(
        &self,
        req: Request<BeginPostMediaUploadRequest>,
    ) -> Res<BeginPostMediaUploadResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = if r.change_id.is_empty() {
                self.write_cx(&caller, &repo, "post.media_upload").await?
            } else {
                self.edit_cx(&caller, &repo, &r.change_id, "post.media_upload")
                    .await?
            };
            let kind = PostMediaKind::try_from(r.kind).unwrap_or(PostMediaKind::Unspecified);
            let (rec, url) = self
                .posts
                .begin_media(
                    &cx,
                    &repo,
                    &r.change_id,
                    kind,
                    &r.content_type,
                    r.size_bytes,
                )
                .await?;
            let mut headers: std::collections::HashMap<String, String> =
                url.headers.into_iter().collect();
            headers.insert("content-type".into(), rec.declared_type.clone());
            Ok(BeginPostMediaUploadResponse {
                media_id: rec.id,
                upload_url: url.url,
                upload_headers: headers,
                expires_at: Some(ts(url.expires_ms)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn finalize_post_media(&self, req: Request<FinalizePostMediaRequest>) -> Res<PostMedia> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let read = self.read_cx(&caller, &repo).await?;
            let owner = self.posts.media_owner(&read, &repo, &r.media_id).await?;
            let cx = self.media_cx(&caller, &repo, &owner, &r.media_id).await?;
            let rec = self
                .posts
                .finalize_media(
                    &cx,
                    &repo,
                    &r.media_id,
                    &r.sha256,
                    &r.alt_text,
                    r.width,
                    r.height,
                    r.duration_ms,
                )
                .await?;
            let view = View::new(self.forge(), &self.posts, &caller);
            let url = view.media_url(&rec);
            Ok(media_view(&rec, url))
        }
        .await;
        reply(&caller, out)
    }

    async fn approve_post(&self, req: Request<ApprovePostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _, _, _) = self.located(&caller, &r.repo, &r.post_id).await?;
            let write = self
                .forge()
                .authz
                .repo(&caller, &repo, Action::Commit, &[], None, None)
                .await;
            let cx = self
                .base
                .guarded(&caller, &repo.org_id, &repo.id, "post.approve", write)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            if caller.cx.actor_kind != PrincipalKind::Human {
                return Err(ForgeError::permission_denied(
                    "only a person can approve a post; agents and services cannot",
                )
                .with_subcode("human_required"));
            }
            let (rec, v) = self.posts.approve(&cx, &repo, &r.post_id).await?;
            self.serve(&caller, &rec, v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn withdraw_post(&self, req: Request<WithdrawPostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, read, rec, _) = self.located(&caller, &r.repo, &r.post_id).await?;
            if !caller.token.allows_action(Action::Commit) {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "token does not allow `commit`",
                ));
            }
            let cx = if rec.author_id() == caller.cx.actor_id {
                read
            } else {
                let admin = self
                    .forge()
                    .authz
                    .repo(
                        &caller,
                        &repo,
                        Action::Commit,
                        &[],
                        None,
                        Some(RepoRole::Admin),
                    )
                    .await;
                self.base
                    .guarded(&caller, &repo.org_id, &repo.id, "post.withdraw", admin)
                    .await?
            };
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let (rec, v) = self
                .posts
                .withdraw(&cx, &repo, &r.post_id, &r.reason)
                .await?;
            self.serve(&caller, &rec, v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_post(&self, req: Request<GetPostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, _, rec, v) = self.located(&caller, &None, &r.post_id).await?;
            self.serve(&caller, &rec, v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_posts(
        &self,
        req: Request<ListRepoPostsRequest>,
    ) -> Res<ListRepoPostsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let (token, limit) = page(&r.page);
            let before = feed::parse_cursor(&token)?;
            let (rows, nt) = self.posts.repo_page(&cx, &repo.id, before, limit).await?;
            let mut view = View::new(self.forge(), &self.posts, &caller);
            let mut posts = Vec::with_capacity(rows.len());
            for (rec, v) in &rows {
                posts.push(view.post(rec, *v).await?);
            }
            Ok(ListRepoPostsResponse {
                posts,
                page: next(nt.as_ref().map(feed::cursor_text).unwrap_or_default()),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn like_post(&self, req: Request<LikePostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self.engage(&caller, &r.post_id, true, false).await;
        reply(&caller, out)
    }

    async fn unlike_post(&self, req: Request<UnlikePostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self.engage(&caller, &r.post_id, false, false).await;
        reply(&caller, out)
    }

    async fn repost_post(&self, req: Request<RepostPostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self.engage(&caller, &r.post_id, true, true).await;
        reply(&caller, out)
    }

    async fn undo_repost(&self, req: Request<UndoRepostRequest>) -> Res<Post> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self.engage(&caller, &r.post_id, false, true).await;
        reply(&caller, out)
    }

    async fn list_following_feed(
        &self,
        req: Request<ListFollowingFeedRequest>,
    ) -> Res<ListFollowingFeedResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out =
            async {
                if caller.is_anonymous() {
                    return Err(ForgeError::new(
                        ErrorReason::Unauthenticated,
                        "sign in to read your feed",
                    ));
                }
                let cx = self.base.user_cx(&caller, Action::Read)?;
                let me = caller.cx.actor_id.clone();
                let (token, limit) = page(&r.page);
                let after = feed::parse_cursor(&r.after)?;
                let (start, dir) = match &after {
                    Some(a) => (Some(a.clone()), Dir::Newer),
                    None => (feed::parse_cursor(&token)?, Dir::Older),
                };
                let users = &self.base.community().users;
                let (repos, _) = users
                    .followed_repos(&cx, &me, "", feed::MAX_SOURCES)
                    .await?;
                let (people, _) = users.following(&cx, &me, "", feed::MAX_SOURCES).await?;
                let sources = Sources {
                    repos: repos.into_iter().map(|(id, _)| id).collect(),
                    people: people.into_iter().collect::<BTreeSet<String>>(),
                };
                let mut view = View::new(self.forge(), &self.posts, &caller);
                let page = self
                    .posts
                    .feed(&cx, &sources, start, dir, limit, &mut view)
                    .await?;
                let mut items = Vec::with_capacity(page.items.len());
                let mut newest: Option<At> = None;
                for (item, rec, v) in &page.items {
                    let key = (item.at, item.post_id.clone());
                    newest = Some(match newest {
                        Some(n) => n.max(key),
                        None => key,
                    });
                    let reposted_by =
                        match &item.reason {
                            feed::Reason::Repost(who) => {
                                Some(self.forge().hooks.directory.get(who).await.unwrap_or_else(
                                    || crate::community::common::principal_ref(who),
                                ))
                            }
                            feed::Reason::Published => None,
                        };
                    items.push(FeedItem {
                        post: Some(view.post(rec, *v).await?),
                        reposted_by,
                        at: Some(ts(item.at.max(0) as u64)),
                    });
                }
                let newest_cursor = match (newest, &after) {
                    (Some(n), _) => feed::cursor_text(&n),
                    (None, Some(a)) => feed::cursor_text(a),
                    // An empty feed still hands out a position to poll from: now, before any id.
                    (None, None) => feed::cursor_text(&(self.posts.db.now_ms() as i64, "0".into())),
                };
                // Polling pages forward through `newest_cursor`; only the newest-first listing
                // pages backward through the page token.
                let nt = match dir {
                    Dir::Older => page
                        .next
                        .as_ref()
                        .map(feed::cursor_text)
                        .unwrap_or_default(),
                    Dir::Newer => String::new(),
                };
                Ok(ListFollowingFeedResponse {
                    items,
                    page: next(nt),
                    newest_cursor,
                })
            }
            .await;
        reply(&caller, out)
    }

    async fn list_post_adoptions(
        &self,
        req: Request<ListPostAdoptionsRequest>,
    ) -> Res<ListPostAdoptionsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, cx, _, _) = self.located(&caller, &None, &r.post_id).await?;
            let (token, limit) = page(&r.page);
            let cursor = adopt::parse_cursor(&token)?;
            let mut view = View::new(self.forge(), &self.posts, &caller);
            let mut rows: Vec<AdoptionRec> = view
                .readable_adoptions(&cx, &r.post_id)
                .await?
                .into_iter()
                .filter(|a| adopt::after_cursor(a, &cursor))
                .collect();
            let more = rows.len() > limit;
            rows.truncate(limit);
            let nt = if more {
                rows.last().map(adopt::cursor_of).unwrap_or_default()
            } else {
                String::new()
            };
            let mut adoptions = Vec::with_capacity(rows.len());
            for a in self.posts.live(&cx, rows).await? {
                adoptions.push(view.adoption(&a).await);
            }
            Ok(ListPostAdoptionsResponse {
                adoptions,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }
}
