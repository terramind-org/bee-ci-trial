//! [`Posts`]: posts of their own and the post of each change request, their media, approval,
//! withdrawal, likes and reposts, the publishing hooks the change-request lifecycle calls
//! inside its own transactions, and the upkeep that deletes unattached media.
//!
//! Callers authorize first (`service.rs`): these methods check the post's own rules (state,
//! text, media, who may approve) but not repo roles.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::{
    ChangeState, Event, Post, PostArticle, PostAttachment, PostMedia, PostMediaKind, PostState,
    Principal, Repo, RepoRef, SubjectKind, SubjectRef, event, repo_ref,
};
use bytes::Bytes;

use super::model::PostRec;
use super::{
    COUNTERS, LIKES, MAX_IMAGES, MAX_REASON_CHARS, MEDIA, MEDIA_PREFIX, POST_IDS, POSTS, REPOSTS,
    UNATTACHED_TTL_MS, text,
};
use crate::audit::AuditDraft;
use crate::changes::model::CrRecord;
use crate::changes::store::Changes;
use crate::changes::{CRS, SURFACE};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::hooks::PrincipalDirectory;
use crate::ids::{ts, ts_ms};
use crate::media::{IMAGE, Limit, MediaRec, MediaSigner, UploadState, Uploads, VIDEO};
use crate::packages::ports::{BlobPort, MemoryBlobs};
use crate::store::{Cursor, Db, Key, Row, Scan, Tx};

/// What the posts module needs beyond the forge: where media bytes live and how their URLs
/// are signed.
#[derive(Clone)]
pub struct PostPorts {
    /// Media bytes.
    pub blobs: Arc<dyn BlobPort>,
    /// Media upload and download URLs.
    pub signer: MediaSigner,
}

impl Default for PostPorts {
    fn default() -> PostPorts {
        PostPorts {
            blobs: Arc::new(MemoryBlobs::new()),
            signer: MediaSigner::random("https://media.beehive.invalid"),
        }
    }
}

/// Engagement of one post as one viewer sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Engagement {
    /// Likes.
    pub likes: u64,
    /// Reposts.
    pub reposts: u64,
    /// Comments on the change request.
    pub comments: u64,
    /// The viewer liked it.
    pub liked: bool,
    /// The viewer reposted it.
    pub reposted: bool,
    /// The change request's author.
    pub change_author: String,
}

/// What a post says beside its summary text and media, as a writer sent it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Content {
    /// The Markdown body; empty for none.
    pub body_markdown: String,
    /// The article, if any.
    pub article: Option<PostArticle>,
    /// The attachments: `kind` and `id` (or `number`), see [`super::attach`].
    pub attachments: Vec<PostAttachment>,
}

/// The media owner (`MediaRec::owner`) of uploads for a post of its own: `@<uploader>`. Media
/// for a change request's post is owned by the change request id, which never starts with `@`.
pub fn own_media_owner(uploader: &str) -> String {
    format!("@{uploader}")
}

/// Whether a media owner names an uploader ([`own_media_owner`]) rather than a change request.
pub fn is_own_media_owner(owner: &str) -> bool {
    owner.starts_with('@')
}

/// A position in a time-ordered list of posts: `(ms, post id)`.
pub type At = (i64, String);

/// Posts over the forge store.
pub struct Posts {
    pub(crate) db: Db,
    pub(crate) uploads: Uploads,
    pub(crate) directory: Arc<dyn PrincipalDirectory>,
}

fn repo_id_of(p: &Post) -> String {
    crate::community::common::ref_repo_id(&p.repo)
}

fn repo_ref(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    })
}

fn state_name(s: PostState) -> &'static str {
    s.as_str_name()
}

/// The row of a post: `n1` is set only while it is published, so repo and author scans by
/// `n1` see published posts alone. `k2` (unique in the repo) is the change request that
/// carries the post, unset for a post of its own.
fn post_row(rec: &PostRec, version: u64) -> Row {
    let p = rec.post();
    let mut row = Row::new(repo_id_of(p), p.id.clone())
        .k1(rec.author_id().to_string())
        .version(version)
        .msg(rec);
    if !p.change_id.is_empty() {
        row.k2 = Some(p.change_id.clone());
    }
    if rec.state() == PostState::Published {
        row.n1 = Some(p.published_at.as_ref().map(ts_ms).unwrap_or(0) as i64);
    }
    row
}

/// The row of a media record: `k2` names the post it is attached to; `n1` (since when it is
/// unattached) only while unattached, so upkeep scans unattached media alone.
fn media_row(rec: &MediaRec, unattached_since: Option<i64>, version: u64) -> Row {
    let mut row = Row::new(rec.scope.clone(), rec.id.clone())
        .k1(rec.owner.clone())
        .version(version)
        .msg(rec);
    if rec.attached.is_empty() {
        row.n1 = Some(unattached_since.unwrap_or(rec.created_ms as i64));
    } else {
        row.k2 = Some(rec.attached.clone());
    }
    row
}

/// The media as a post carries it (no URL: those are signed per read).
fn post_media(rec: &MediaRec) -> PostMedia {
    let kind = if rec.class == VIDEO.name {
        PostMediaKind::Video
    } else {
        PostMediaKind::Image
    };
    PostMedia {
        id: rec.id.clone(),
        kind: kind as i32,
        content_type: rec.content_type.clone(),
        size_bytes: rec.size,
        width: rec.width,
        height: rec.height,
        duration_ms: rec.duration_ms,
        alt_text: rec.alt_text.clone(),
        sha256: rec.sha256.clone(),
        url: String::new(),
    }
}

/// The post as events carry it: a withdrawn post's words and media are gone.
pub(crate) fn event_view(rec: &PostRec) -> Post {
    let mut p = rec.post().clone();
    if rec.state() == PostState::Withdrawn {
        p.text.clear();
        p.media.clear();
        p.body_markdown.clear();
        p.article = None;
    }
    p
}

fn post_json(rec: &PostRec) -> String {
    let p = rec.post();
    serde_json::json!({
        "id": p.id,
        "change_id": p.change_id,
        "change_number": p.change_number,
        "state": state_name(rec.state()),
        "author": rec.author_id(),
        "approved_by": p.approved_by.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
        "media": p.media.len(),
        "body_chars": p.body_markdown.chars().count(),
        "article": p.article.as_ref().map(|a| a.title.clone()).unwrap_or_default(),
        "attachments": super::attach::refs_of(p).len(),
    })
    .to_string()
}

async fn emit(tx: &mut Tx, kind: &str, rec: &PostRec, payload: serde_json::Value) -> Result<()> {
    let p = rec.post();
    let repo_id = repo_id_of(p);
    tx.emit_event(Event {
        kind: kind.into(),
        repo_id: repo_id.clone(),
        org_id: rec.org_id.clone(),
        body: Some(event::Body::Post(Box::new(event_view(rec)))),
        subject: Some(SubjectRef {
            kind: SubjectKind::Post as i32,
            repo: repo_ref(&repo_id),
            id: p.id.clone(),
        }),
        payload_json: if payload.is_null() {
            String::new()
        } else {
            payload.to_string()
        },
        ..Default::default()
    })
    .await
}

fn audit(action: &str, rec: &PostRec) -> AuditDraft {
    let p = rec.post();
    AuditDraft::new(action, &rec.org_id)
        .repo_id(&repo_id_of(p))
        .target(SubjectKind::Post, &p.id)
        .after(&post_json(rec))
}

async fn by_change(tx: &mut Tx, repo_id: &str, change_id: &str) -> Result<Option<(PostRec, Row)>> {
    Ok(tx
        .scan_msgs::<PostRec>(
            &POSTS,
            &Scan::scope(repo_id, 1).key(Key::K2(change_id.to_string())),
        )
        .await?
        .into_iter()
        .next())
}

async fn by_id(tx: &mut Tx, repo_id: &str, post_id: &str) -> Result<(PostRec, Row)> {
    if post_id.is_empty() {
        return Err(ForgeError::missing("post_id"));
    }
    tx.get_msg::<PostRec>(&POSTS, repo_id, post_id)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("post {post_id}")))
}

async fn counter(tx: &mut Tx, post_id: &str, what: &str) -> Result<u64> {
    Ok(tx
        .get(&COUNTERS, post_id, what)
        .await?
        .and_then(|r| r.n1)
        .unwrap_or(0)
        .max(0) as u64)
}

async fn media(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(MediaRec, Row)> {
    if id.is_empty() {
        return Err(ForgeError::missing("media_id"));
    }
    tx.get_msg::<MediaRec>(&MEDIA, repo_id, id)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("post media {id}")))
}

/// `bad_media` unless `media_ids` may be one post's media set: no id twice, at most
/// [`MAX_IMAGES`].
fn media_ids_ok(media_ids: &[String]) -> Result<()> {
    let unique: BTreeSet<&String> = media_ids.iter().collect();
    if unique.len() != media_ids.len() {
        return Err(ForgeError::invalid(
            "bad_media",
            "media_ids lists a media item twice",
        ));
    }
    if media_ids.len() > MAX_IMAGES {
        return Err(ForgeError::invalid(
            "bad_media",
            format!("a post carries at most {MAX_IMAGES} images or one video"),
        ));
    }
    Ok(())
}

/// The media set `media_ids` of a post, in display order: every item finalized and uploaded
/// for `owner` (`for_what` names it in the error), and a video alone.
async fn media_set(
    tx: &mut Tx,
    repo_id: &str,
    media_ids: &[String],
    owner: &str,
    for_what: &str,
) -> Result<Vec<(MediaRec, Row)>> {
    let mut attach: Vec<(MediaRec, Row)> = Vec::new();
    for id in media_ids {
        let (m, row) = media(tx, repo_id, id).await?;
        if m.owner != owner {
            return Err(ForgeError::invalid(
                "bad_media",
                format!("media {id} was uploaded for another {for_what}"),
            ));
        }
        if m.upload_state() != UploadState::Final {
            return Err(ForgeError::invalid_state(format!(
                "media {id} is not finalized (FinalizePostMedia)"
            ))
            .with_subcode("media_not_final"));
        }
        attach.push((m, row));
    }
    let videos = attach.iter().filter(|(m, _)| m.class == VIDEO.name).count();
    if videos > 1 || (videos == 1 && attach.len() > 1) {
        return Err(ForgeError::invalid(
            "bad_media",
            "a video is the only media of its post",
        ));
    }
    Ok(attach)
}

/// Publishes `rec` at `now`.
fn publish(rec: &mut PostRec, now: u64) {
    let p = rec.post_mut();
    p.state = PostState::Published as i32;
    p.published_at = Some(ts(now));
    p.updated_at = Some(ts(now));
}

/// Refreshes what the post repeats of its change request.
fn copy_change(rec: &mut PostRec, cr: &CrRecord) {
    let c = cr.cr();
    let p = rec.post_mut();
    p.change_number = c.number;
    p.change_title.clone_from(&c.title);
}

/// The land hook (`Changes::record_landed`, same transaction): a draft by a person, or one a
/// person already approved, is published at the landing time; any other draft waits for
/// approval (`PENDING_APPROVAL`, `post.approval_requested`).
pub(crate) async fn on_landed(tx: &mut Tx, cr: &CrRecord) -> Result<()> {
    let c = cr.cr();
    let Some((mut rec, row)) = by_change(tx, &c.repo_id, &c.id).await? else {
        return Ok(());
    };
    if rec.state() != PostState::Draft {
        return Ok(());
    }
    let now = c
        .landed_at
        .as_ref()
        .map(ts_ms)
        .unwrap_or_else(|| tx.now_ms());
    copy_change(&mut rec, cr);
    let (kind, action) = if rec.by_human() || rec.post().approved_by.is_some() {
        publish(&mut rec, now);
        ("post.published", "post.publish")
    } else {
        let p = rec.post_mut();
        p.state = PostState::PendingApproval as i32;
        p.updated_at = Some(ts(now));
        ("post.approval_requested", "post.request_approval")
    };
    tx.update(&POSTS, &post_row(&rec, row.version)).await?;
    emit(tx, kind, &rec, serde_json::Value::Null).await?;
    tx.audit(audit(action, &rec)).await
}

/// The close hook (`Changes::close`, same transaction): the draft is discarded.
pub(crate) async fn on_closed(tx: &mut Tx, cr: &CrRecord) -> Result<()> {
    let c = cr.cr();
    let Some((mut rec, row)) = by_change(tx, &c.repo_id, &c.id).await? else {
        return Ok(());
    };
    if rec.state() != PostState::Draft {
        return Ok(());
    }
    let now = tx.now_ms();
    let p = rec.post_mut();
    p.state = PostState::Discarded as i32;
    p.updated_at = Some(ts(now));
    tx.update(&POSTS, &post_row(&rec, row.version)).await?;
    tx.audit(audit("post.discard", &rec)).await
}

/// The reopen hook (`Changes::reopen`, same transaction): a discarded post is a draft again.
pub(crate) async fn on_reopened(tx: &mut Tx, cr: &CrRecord) -> Result<()> {
    let c = cr.cr();
    let Some((mut rec, row)) = by_change(tx, &c.repo_id, &c.id).await? else {
        return Ok(());
    };
    if rec.state() != PostState::Discarded {
        return Ok(());
    }
    let now = tx.now_ms();
    let p = rec.post_mut();
    p.state = PostState::Draft as i32;
    p.updated_at = Some(ts(now));
    tx.update(&POSTS, &post_row(&rec, row.version)).await?;
    tx.audit(audit("post.restore", &rec)).await
}

/// `INVALID_STATE` unless a post may be written for the change request now: its post is a
/// draft or waits for approval, or it has none and the change is open.
fn writable(cr: &CrRecord, post: Option<&PostRec>) -> Result<()> {
    let state = ChangeState::try_from(cr.cr().state).unwrap_or(ChangeState::Unspecified);
    match post {
        Some(p) if p.editable() => Ok(()),
        Some(p) if p.state() == PostState::Discarded && state == ChangeState::Open => Ok(()),
        Some(p) => Err(ForgeError::invalid_state(format!(
            "the post is {} and can no longer change",
            state_name(p.state())
        ))
        .with_subcode("post_not_editable")),
        None if state == ChangeState::Open => Ok(()),
        None => Err(ForgeError::invalid_state(format!(
            "change request #{} is no longer open; posts are written before it lands",
            cr.cr().number
        ))
        .with_subcode("post_not_editable")),
    }
}

impl Posts {
    /// The posts module over `forge`'s store and directory, plus `ports`.
    pub fn new(forge: &Forge, ports: PostPorts) -> Arc<Posts> {
        Arc::new(Posts {
            db: forge.db.clone(),
            uploads: Uploads::new(ports.blobs, ports.signer),
            directory: forge.hooks.directory.clone(),
        })
    }

    /// Installs the module on the change requests, which then publish, discard and restore
    /// posts in their own transactions and carry `ChangeRequest.post` on reads, and on the
    /// issues, which then accept `inspired_by_post_id`.
    pub fn attach(self: &Arc<Self>, changes: &Changes) {
        changes.set_posts(self.clone());
        changes.work.set_posts(self.clone());
    }

    /// The upload flow.
    pub fn uploads(&self) -> &Uploads {
        &self.uploads
    }

    /// The acting principal with display data.
    async fn actor(&self, cx: &Cx) -> Principal {
        let mut p = self
            .directory
            .get(&cx.actor_id)
            .await
            .unwrap_or_else(|| cx.principal());
        if p.kind == 0 {
            p.kind = cx.actor_kind as i32;
        }
        p
    }

    async fn change(tx: &mut Tx, repo: &Repo, change_id: &str) -> Result<CrRecord> {
        Ok(Changes::load(tx, &repo.id, change_id).await?.0)
    }

    // ----- reads ------------------------------------------------------------------------

    /// The repo of a post, found by the post id alone.
    pub async fn locate(&self, cx: &Cx, post_id: &str) -> Result<String> {
        if post_id.is_empty() {
            return Err(ForgeError::missing("post_id"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get(&POST_IDS, "", post_id)
            .await?
            .and_then(|r| r.k1)
            .ok_or_else(|| ForgeError::not_found(format!("post {post_id}")))
    }

    /// A post and its version.
    pub async fn get(&self, cx: &Cx, repo_id: &str, post_id: &str) -> Result<(PostRec, u64)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = by_id(&mut tx, repo_id, post_id).await?;
        Ok((rec, row.version))
    }

    /// The post of a change request, if it has one.
    pub async fn for_change(
        &self,
        cx: &Cx,
        repo_id: &str,
        change_id: &str,
    ) -> Result<Option<(PostRec, u64)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(by_change(&mut tx, repo_id, change_id)
            .await?
            .map(|(r, row)| (r, row.version)))
    }

    /// The posts of several change requests, by change request id.
    pub async fn for_changes(
        &self,
        cx: &Cx,
        repo_id: &str,
        change_ids: &[String],
    ) -> Result<BTreeMap<String, (PostRec, u64)>> {
        let mut out = BTreeMap::new();
        if change_ids.is_empty() {
            return Ok(out);
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for id in change_ids {
            if let Some((rec, row)) = by_change(&mut tx, repo_id, id).await? {
                out.insert(id.clone(), (rec, row.version));
            }
        }
        Ok(out)
    }

    /// A repo's published posts, newest first, strictly older than `before`.
    pub async fn repo_page(
        &self,
        cx: &Cx,
        repo_id: &str,
        before: Option<At>,
        limit: usize,
    ) -> Result<(Vec<(PostRec, u64)>, Option<At>)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo_id, limit + 1).by_n1().desc();
        scan.after = before.map(|(n1, id)| Cursor {
            n1,
            scope: repo_id.to_string(),
            id,
        });
        let mut rows = tx.scan_msgs::<PostRec>(&POSTS, &scan).await?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = if more {
            rows.last().map(|(_, r)| (r.n1.unwrap_or(0), r.id.clone()))
        } else {
            None
        };
        Ok((
            rows.into_iter().map(|(r, row)| (r, row.version)).collect(),
            next,
        ))
    }

    /// Likes, reposts and comments of a post, what `viewer` did, and the change request's
    /// author (empty for a post of its own).
    pub async fn engagement(&self, cx: &Cx, rec: &PostRec, viewer: &str) -> Result<Engagement> {
        let p = rec.post();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let likes = counter(&mut tx, &p.id, "likes").await?;
        let reposts = counter(&mut tx, &p.id, "reposts").await?;
        let liked = tx.get(&LIKES, &p.id, viewer).await?.is_some();
        let reposted = tx.get(&REPOSTS, viewer, &p.id).await?.is_some();
        // A post of its own has no change request: no comments, no change author.
        let cr = if p.change_id.is_empty() {
            None
        } else {
            tx.scan_msgs::<CrRecord>(
                &CRS,
                &Scan::scope(repo_id_of(p), 1).key(Key::K1(p.change_id.clone())),
            )
            .await?
            .into_iter()
            .next()
            .map(|(c, _)| c)
        };
        Ok(Engagement {
            likes,
            reposts,
            comments: cr.as_ref().map(|c| u64::from(c.comment_count)).unwrap_or(0),
            liked,
            reposted,
            change_author: cr
                .as_ref()
                .and_then(|c| c.cr().author.as_ref().map(|a| a.id.clone()))
                .unwrap_or_default(),
        })
    }

    /// The change request's author, for the edit rule.
    pub async fn change_author(&self, cx: &Cx, repo: &Repo, change_id: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let cr = Self::change(&mut tx, repo, change_id).await?;
        Ok(cr
            .cr()
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default())
    }

    /// The change request a media upload belongs to, or its uploader ([`own_media_owner`])
    /// when it is for a post of its own.
    pub async fn media_owner(&self, cx: &Cx, repo: &Repo, media_id: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(media(&mut tx, &repo.id, media_id).await?.0.owner)
    }

    // ----- drafts -----------------------------------------------------------------------

    /// Writes the text and media of a change request's post, creating it as a draft; the
    /// caller becomes its author and any earlier approval is cleared (it approved other
    /// words). A post waiting for approval that a person rewrites is theirs and publishes
    /// now. `expected_version` 0 skips the version check. The body, article and attachments
    /// are removed: see [`Posts::set_with`].
    pub async fn set(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        text: &str,
        media_ids: &[String],
        expected_version: u64,
    ) -> Result<(PostRec, u64)> {
        self.set_with(
            cx,
            repo,
            change_id,
            text,
            media_ids,
            &Content::default(),
            expected_version,
        )
        .await
    }

    /// The body, article and attachments of `content` as stored, for a post in `repo` that
    /// `own_change` carries (empty for a post of its own).
    async fn stored(
        &self,
        cx: &Cx,
        repo: &Repo,
        content: &Content,
        own_change: &str,
    ) -> Result<Content> {
        Ok(Content {
            body_markdown: text::body(&content.body_markdown)?,
            article: text::article(content.article.as_ref())?,
            attachments: self
                .resolve_attachments(cx, &repo.id, &content.attachments, own_change)
                .await?,
        })
    }

    /// [`Posts::set`] with a Markdown body, an article and attachments, which replace the
    /// post's own as the text and media do.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_with(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        text: &str,
        media_ids: &[String],
        content: &Content,
        expected_version: u64,
    ) -> Result<(PostRec, u64)> {
        // Before the transaction below: attachments are read on their own surfaces.
        let content = self.stored(cx, repo, content, change_id).await?;
        let wordless = content.body_markdown.is_empty() && content.article.is_none();
        let text = text::normalize(text, !media_ids.is_empty() || !wordless)?;
        media_ids_ok(media_ids)?;
        let author = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let cr = Self::change(&mut tx, repo, change_id).await?;
        let existing = by_change(&mut tx, &repo.id, change_id).await?;
        writable(&cr, existing.as_ref().map(|(r, _)| r))?;
        let current = existing.as_ref().map(|(_, r)| r.version).unwrap_or(0);
        if expected_version != 0 && expected_version != current {
            return Err(ForgeError::version_mismatch(expected_version, current));
        }
        let now = tx.now_ms();
        let post_id = match &existing {
            Some((r, _)) => r.post().id.clone(),
            None => tx.new_id()?,
        };
        // The media set, in display order.
        let attach = media_set(&mut tx, &repo.id, media_ids, change_id, "change request").await?;
        let landed = ChangeState::try_from(cr.cr().state) == Ok(ChangeState::Landed);
        let mut rec = existing
            .as_ref()
            .map(|(r, _)| r.clone())
            .unwrap_or_else(|| PostRec {
                post: Some(Post {
                    id: post_id.clone(),
                    repo: repo_ref(&repo.id),
                    change_id: change_id.to_string(),
                    created_at: Some(ts(now)),
                    state: PostState::Draft as i32,
                    ..Default::default()
                }),
                org_id: repo.org_id.clone(),
            });
        let old_media: Vec<String> = rec.post().media.iter().map(|m| m.id.clone()).collect();
        copy_change(&mut rec, &cr);
        {
            let p = rec.post_mut();
            p.author = Some(author);
            p.text = text;
            p.media = attach.iter().map(|(m, _)| post_media(m)).collect();
            p.body_markdown = content.body_markdown;
            p.article = content.article;
            p.attachments = content.attachments;
            p.approved_by = None;
            p.updated_at = Some(ts(now));
            if p.state == PostState::Discarded as i32 {
                p.state = PostState::Draft as i32;
            }
        }
        let publish_now = landed && rec.state() == PostState::PendingApproval && rec.by_human();
        if publish_now {
            publish(&mut rec, now);
        }
        let version = match &existing {
            Some((_, row)) => tx.update(&POSTS, &post_row(&rec, row.version)).await?,
            None => {
                tx.insert(&POSTS, &post_row(&rec, 0)).await?;
                tx.insert(
                    &POST_IDS,
                    &Row::new("", post_id.clone()).k1(repo.id.clone()),
                )
                .await?;
                1
            }
        };
        for (mut m, row) in attach {
            if m.attached != post_id {
                m.attached.clone_from(&post_id);
                tx.update(&MEDIA, &media_row(&m, None, row.version)).await?;
            }
        }
        for id in old_media {
            if media_ids.contains(&id) {
                continue;
            }
            if let Ok((mut m, row)) = media(&mut tx, &repo.id, &id).await {
                m.attached.clear();
                tx.update(&MEDIA, &media_row(&m, Some(now as i64), row.version))
                    .await?;
            }
        }
        if publish_now {
            emit(&mut tx, "post.published", &rec, serde_json::Value::Null).await?;
        } else {
            emit(&mut tx, "post.drafted", &rec, serde_json::Value::Null).await?;
        }
        tx.audit(audit("post.set", &rec)).await?;
        tx.commit().await?;
        Ok((rec, version))
    }

    /// Writes a post of its own in `repo`: no change request carries it (the service checked
    /// the caller holds `commit` there). The caller is its author. A person's post is
    /// published now; one by an agent or service waits for a person's approval
    /// (`PENDING_APPROVAL`, `post.approval_requested`). `media_ids` are finalized uploads of
    /// the caller for a post of its own ([`own_media_owner`]) that no post carries yet.
    pub async fn create(
        &self,
        cx: &Cx,
        repo: &Repo,
        text: &str,
        media_ids: &[String],
        content: &Content,
    ) -> Result<(PostRec, u64)> {
        // Before the transaction below: attachments are read on their own surfaces.
        let content = self.stored(cx, repo, content, "").await?;
        let wordless = content.body_markdown.is_empty() && content.article.is_none();
        let text = text::normalize(text, !media_ids.is_empty() || !wordless)?;
        media_ids_ok(media_ids)?;
        let author = self.actor(cx).await;
        let owner = own_media_owner(&cx.actor_id);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let post_id = tx.new_id()?;
        let attach = media_set(&mut tx, &repo.id, media_ids, &owner, "author or post").await?;
        if let Some((m, _)) = attach.iter().find(|(m, _)| !m.attached.is_empty()) {
            return Err(ForgeError::invalid(
                "bad_media",
                format!("media {} is already on another post", m.id),
            ));
        }
        let mut rec = PostRec {
            post: Some(Post {
                id: post_id.clone(),
                repo: repo_ref(&repo.id),
                author: Some(author),
                text,
                media: attach.iter().map(|(m, _)| post_media(m)).collect(),
                body_markdown: content.body_markdown,
                article: content.article,
                attachments: content.attachments,
                state: PostState::PendingApproval as i32,
                created_at: Some(ts(now)),
                updated_at: Some(ts(now)),
                ..Default::default()
            }),
            org_id: repo.org_id.clone(),
        };
        let published = rec.by_human();
        if published {
            publish(&mut rec, now);
        }
        tx.insert(&POSTS, &post_row(&rec, 0)).await?;
        tx.insert(
            &POST_IDS,
            &Row::new("", post_id.clone()).k1(repo.id.clone()),
        )
        .await?;
        for (mut m, row) in attach {
            m.attached.clone_from(&post_id);
            tx.update(&MEDIA, &media_row(&m, None, row.version)).await?;
        }
        let kind = if published {
            "post.published"
        } else {
            "post.approval_requested"
        };
        emit(&mut tx, kind, &rec, serde_json::Value::Null).await?;
        tx.audit(audit("post.create", &rec)).await?;
        tx.commit().await?;
        Ok((rec, 1))
    }

    /// Removes a change request's post (a draft or one waiting for approval) and its media.
    pub async fn clear(&self, cx: &Cx, repo: &Repo, change_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let cr = Self::change(&mut tx, repo, change_id).await?;
        let Some((rec, _)) = by_change(&mut tx, &repo.id, change_id).await? else {
            return Err(ForgeError::not_found(format!(
                "a post on change request #{}",
                cr.cr().number
            )));
        };
        if !rec.editable() && rec.state() != PostState::Discarded {
            return Err(ForgeError::invalid_state(format!(
                "the post is {}; withdraw it instead",
                state_name(rec.state())
            ))
            .with_subcode("post_not_editable"));
        }
        let p = rec.post().clone();
        tx.delete(&POSTS, &repo.id, &p.id).await?;
        tx.delete(&POST_IDS, "", &p.id).await?;
        let mut gone = Vec::new();
        for m in &p.media {
            if let Ok((m, _)) = media(&mut tx, &repo.id, &m.id).await {
                tx.delete(&MEDIA, &repo.id, &m.id).await?;
                gone.push(m);
            }
        }
        tx.audit(audit("post.clear", &rec)).await?;
        tx.commit().await?;
        for m in gone {
            self.uploads.discard(&m).await?;
        }
        Ok(())
    }

    // ----- media ------------------------------------------------------------------------

    /// Starts a media upload for a change request's post, or with an empty `change_id` for a
    /// post of its own (the upload then belongs to its uploader, [`own_media_owner`]). `kind`
    /// unspecified is taken from the content type.
    pub async fn begin_media(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        kind: PostMediaKind,
        content_type: &str,
        size: u64,
    ) -> Result<(MediaRec, crate::community::ports::SignedUrl)> {
        let limit: Limit = match kind {
            PostMediaKind::Image => IMAGE,
            PostMediaKind::Video => VIDEO,
            PostMediaKind::Unspecified => match crate::media::Kind::from_content_type(content_type)
            {
                Some(k) if !k.is_image() => VIDEO,
                _ => IMAGE,
            },
        };
        let uploader = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let owner = if change_id.is_empty() {
            own_media_owner(&cx.actor_id)
        } else {
            let cr = Self::change(&mut tx, repo, change_id).await?;
            let existing = by_change(&mut tx, &repo.id, change_id).await?;
            writable(&cr, existing.as_ref().map(|(r, _)| r))?;
            change_id.to_string()
        };
        let id = tx.new_id()?;
        let now = tx.now_ms();
        let (rec, url) = self.uploads.begin(
            MEDIA_PREFIX,
            &id,
            &repo.id,
            &owner,
            limit,
            content_type,
            size,
            uploader,
            now,
        )?;
        tx.insert(&MEDIA, &media_row(&rec, None, 0)).await?;
        tx.commit().await?;
        Ok((rec, url))
    }

    /// Stores the bytes of a pending upload (the signed `PUT` URL's handler): they must
    /// arrive in time, fit the class and sniff as one of its kinds.
    pub async fn receive_media(
        &self,
        cx: &Cx,
        repo_id: &str,
        media_id: &str,
        bytes: Bytes,
    ) -> Result<()> {
        let mut rec = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            media(&mut tx, repo_id, media_id).await?.0
        };
        let now = self.db.now_ms();
        self.uploads.receive(&mut rec, bytes, now).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (cur, row) = media(&mut tx, repo_id, media_id).await?;
        if cur.upload_state() == UploadState::Final {
            return Err(ForgeError::invalid_state("the upload is already finalized"));
        }
        tx.update(&MEDIA, &media_row(&rec, row.n1, row.version))
            .await?;
        tx.commit().await
    }

    /// Finalizes an upload: the client's SHA-256 must match the server's (`INTEGRITY`), and an
    /// image needs alt text. Image dimensions are the ones read from the bytes; a video's are
    /// as declared.
    #[allow(clippy::too_many_arguments)]
    pub async fn finalize_media(
        &self,
        cx: &Cx,
        repo: &Repo,
        media_id: &str,
        sha256: &str,
        alt_text: &str,
        width: u32,
        height: u32,
        duration_ms: u32,
    ) -> Result<MediaRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = media(&mut tx, &repo.id, media_id).await?;
        let image = rec.class == IMAGE.name;
        let alt = text::alt_text(alt_text, image)?;
        let was_final = rec.upload_state() == UploadState::Final;
        self.uploads.finalize(&mut rec, sha256, tx.now_ms())?;
        if was_final {
            return Ok(rec);
        }
        rec.alt_text = alt;
        if !image {
            rec.width = width;
            rec.height = height;
            rec.duration_ms = duration_ms;
        }
        tx.update(&MEDIA, &media_row(&rec, row.n1, row.version))
            .await?;
        tx.audit(
            AuditDraft::new("post.media_upload", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(&rec.id)
                .after(
                    &serde_json::json!({
                        "change_id": rec.owner,
                        "content_type": rec.content_type,
                        "size": rec.size,
                        "sha256": rec.sha256,
                    })
                    .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(rec)
    }

    /// Finalized media to serve: `NOT_FOUND` while it is uploading, and once the post it is on
    /// was withdrawn or removed.
    pub async fn media_for_download(
        &self,
        cx: &Cx,
        repo_id: &str,
        media_id: &str,
    ) -> Result<MediaRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = media(&mut tx, repo_id, media_id).await?;
        if rec.upload_state() != UploadState::Final {
            return Err(ForgeError::not_found(format!("post media {media_id}")));
        }
        if !rec.attached.is_empty() {
            match tx
                .get_msg::<PostRec>(&POSTS, repo_id, &rec.attached)
                .await?
            {
                Some((p, _)) if p.state() != PostState::Withdrawn => {}
                _ => return Err(ForgeError::not_found(format!("post media {media_id}"))),
            }
        }
        Ok(rec)
    }

    /// The bytes of media.
    pub async fn media_bytes(&self, rec: &MediaRec) -> Result<Bytes> {
        self.uploads
            .bytes(rec)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("post media {}", rec.id)))
    }

    // ----- approval and withdrawal ------------------------------------------------------

    /// Approves an agent- or service-written post as the calling person (the service checked
    /// they hold repo Write): before the change lands it records `approved_by`; once it
    /// landed, and for a post of its own, the post publishes now.
    pub async fn approve(&self, cx: &Cx, repo: &Repo, post_id: &str) -> Result<(PostRec, u64)> {
        let approver = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = by_id(&mut tx, &repo.id, post_id).await?;
        if rec.author_id() == cx.actor_id {
            return Err(ForgeError::permission_denied(
                "the author of a post cannot approve it; another person must",
            )
            .with_subcode("own_post"));
        }
        if rec.by_human() {
            return Err(
                ForgeError::invalid_state("a post written by a person needs no approval")
                    .with_subcode("approval_not_needed"),
            );
        }
        let now = tx.now_ms();
        let published = match rec.state() {
            PostState::Draft => {
                let p = rec.post_mut();
                p.approved_by = Some(approver);
                p.updated_at = Some(ts(now));
                false
            }
            PostState::PendingApproval => {
                rec.post_mut().approved_by = Some(approver);
                publish(&mut rec, now);
                true
            }
            s => {
                return Err(ForgeError::invalid_state(format!(
                    "the post is {}",
                    state_name(s)
                )));
            }
        };
        let v = tx.update(&POSTS, &post_row(&rec, row.version)).await?;
        if published {
            emit(&mut tx, "post.published", &rec, serde_json::Value::Null).await?;
        }
        tx.audit(audit("post.approve", &rec)).await?;
        tx.commit().await?;
        Ok((rec, v))
    }

    /// Withdraws a post that waits for approval or is published (the service checked the
    /// caller is its author or a repo admin): it leaves every feed and its media is deleted.
    /// Withdrawing again is a no-op.
    pub async fn withdraw(
        &self,
        cx: &Cx,
        repo: &Repo,
        post_id: &str,
        reason: &str,
    ) -> Result<(PostRec, u64)> {
        let n = reason.chars().count();
        if n > MAX_REASON_CHARS {
            return Err(ForgeError::too_large(
                "withdrawn_reason",
                MAX_REASON_CHARS as u64,
                n as u64,
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = by_id(&mut tx, &repo.id, post_id).await?;
        match rec.state() {
            PostState::PendingApproval | PostState::Published => {}
            PostState::Withdrawn => return Ok((rec, row.version)),
            s => {
                return Err(ForgeError::invalid_state(format!(
                    "the post is {}; a draft is cleared, not withdrawn",
                    state_name(s)
                ))
                .with_subcode("post_not_published"));
            }
        }
        let now = tx.now_ms();
        let media_ids: Vec<String> = rec.post().media.iter().map(|m| m.id.clone()).collect();
        {
            let p = rec.post_mut();
            p.state = PostState::Withdrawn as i32;
            p.withdrawn_reason = reason.trim().to_string();
            p.updated_at = Some(ts(now));
        }
        let v = tx.update(&POSTS, &post_row(&rec, row.version)).await?;
        let mut gone = Vec::new();
        for id in media_ids {
            if let Ok((m, _)) = media(&mut tx, &repo.id, &id).await {
                tx.delete(&MEDIA, &repo.id, &id).await?;
                gone.push(m);
            }
        }
        emit(
            &mut tx,
            "post.withdrawn",
            &rec,
            serde_json::json!({"reason": rec.post().withdrawn_reason}),
        )
        .await?;
        tx.audit(audit("post.withdraw", &rec)).await?;
        tx.commit().await?;
        for m in gone {
            self.uploads.discard(&m).await?;
        }
        Ok((rec, v))
    }

    // ----- likes and reposts ------------------------------------------------------------

    /// Likes (`on`) or unlikes a post as the caller. Only published posts can be liked;
    /// liking twice and unliking a post not liked are no-ops.
    pub async fn like(&self, cx: &Cx, repo_id: &str, post_id: &str, on: bool) -> Result<()> {
        self.engage(cx, repo_id, post_id, on, false).await
    }

    /// Reposts (`on`) or undoes a repost as the caller, with the same rules as likes.
    pub async fn repost(&self, cx: &Cx, repo_id: &str, post_id: &str, on: bool) -> Result<()> {
        self.engage(cx, repo_id, post_id, on, true).await
    }

    async fn engage(
        &self,
        cx: &Cx,
        repo_id: &str,
        post_id: &str,
        on: bool,
        repost: bool,
    ) -> Result<()> {
        let me = cx.actor_id.clone();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = by_id(&mut tx, repo_id, post_id).await?;
        let (scope, id) = if repost {
            (me.clone(), post_id.to_string())
        } else {
            (post_id.to_string(), me.clone())
        };
        let (table, counter) = if repost {
            (&REPOSTS, "reposts")
        } else {
            (&LIKES, "likes")
        };
        let exists = tx.get(table, &scope, &id).await?.is_some();
        let now = tx.now_ms();
        let action = match (on, exists) {
            (true, true) | (false, false) => return Ok(()),
            (true, false) => {
                if rec.state() != PostState::Published {
                    return Err(ForgeError::invalid_state(format!(
                        "the post is {}; only published posts can be {}",
                        state_name(rec.state()),
                        if repost { "reposted" } else { "liked" }
                    ))
                    .with_subcode("post_not_published"));
                }
                let mut row = Row::new(scope, id).n1(now as i64);
                if repost {
                    row = row.k1(post_id.to_string()).k2(repo_id.to_string());
                }
                tx.insert(table, &row).await?;
                tx.add_n1(&COUNTERS, post_id, counter, 1).await?;
                let kind = if repost {
                    "post.reposted"
                } else {
                    "post.liked"
                };
                emit(&mut tx, kind, &rec, serde_json::json!({"by": me})).await?;
                if repost { "post.repost" } else { "post.like" }
            }
            (false, true) => {
                tx.delete(table, &scope, &id).await?;
                tx.add_n1(&COUNTERS, post_id, counter, -1).await?;
                if repost {
                    "post.unrepost"
                } else {
                    "post.unlike"
                }
            }
        };
        tx.audit(audit(action, &rec)).await?;
        tx.commit().await
    }

    // ----- upkeep -----------------------------------------------------------------------

    /// Deletes media that has been unattached for [`UNATTACHED_TTL_MS`] (uploads never
    /// attached, expired pending uploads, media removed from a draft). Returns how many.
    pub async fn maintain(&self, cx: &Cx) -> Result<usize> {
        let cutoff = self.db.now_ms().saturating_sub(UNATTACHED_TTL_MS) as i64;
        let stale = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let scan = Scan {
                order: crate::store::Order::N1,
                n1_max: Some(cutoff),
                limit: 1000,
                ..Scan::default()
            };
            tx.scan_msgs::<MediaRec>(&MEDIA, &scan).await?
        };
        let mut n = 0;
        for (rec, row) in stale {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            // Re-read: it may have been attached since.
            match tx.get(&MEDIA, &row.scope, &row.id).await? {
                Some(cur) if cur.k2.is_none() && cur.n1.is_some_and(|t| t <= cutoff) => {
                    tx.delete(&MEDIA, &row.scope, &row.id).await?;
                    tx.commit().await?;
                    self.uploads.discard(&rec).await?;
                    n += 1;
                }
                _ => {}
            }
        }
        Ok(n)
    }
}
