//! Wikis (PRODUCT §4.16, F416-023): each wiki is a sibling repo `<repo>.wiki` in the VCS and
//! pages are files in it, so a wiki is cloneable and its history is the page history.
//!
//! The forge keeps only the wiki's switch and edit policy ([`WikiRec`]); everything else
//! goes through the [`ContentVcs`](super::ports::ContentVcs) port:
//!
//! - a page is a file `<slug>.<ext>` at the wiki root; the extension gives the markup
//!   format (`md`, `adoc`, `rst`, `org`, `textile`, `mediawiki`);
//! - the slug is the title with spaces turned into `-` (`Getting Started` becomes
//!   `Getting-Started`); `Home` is the landing page; `_Sidebar` and `_Footer` are special
//!   pages and never listed;
//! - every create, edit, rename and delete is one changeset authored by the caller, and
//!   updates carry the page's last changeset as an optimistic-concurrency token.
//!
//! State changes emit `wiki.page_updated` (GitHub's `gollum`) with the action in the payload.
//! The event and audit entry are written after the VCS commit succeeds; the VCS history is
//! the authority if the process dies in between.

use std::sync::Arc;

use bee_proto::v1::{
    ObjectId, Repo, SubjectKind, WikiEditAccess, WikiInfo, WikiPage, WikiPageFormat,
    WikiPageRevision, WikiPageSummary, WikiSearchHit,
};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::module::EventKind;
use crate::store::{Db, Row, TableDef};

use super::SURFACE;
use super::common::{MAX_BODY, MAX_TITLE, draft, emit, max_len, page_vec, required, subject};
use super::ports::{CommitInfo, CommitRequest, ContentVcs, FileChange, FileEntry, VcsRepo};

/// Wiki switches, scoped by `""`, keyed by repo id.
pub static WIKIS: TableDef = TableDef {
    name: "forge_wikis",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Wiki event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::gh("wiki.page_updated", "A wiki page changed", "gollum", ""),
    EventKind::new("wiki.enabled", "A wiki was enabled"),
    EventKind::new("wiki.disabled", "A wiki was disabled"),
];

/// The stored wiki switch.
#[derive(Clone, PartialEq, Message)]
pub struct WikiRec {
    /// Enabled.
    #[prost(bool, tag = "1")]
    pub enabled: bool,
    /// [`WikiEditAccess`].
    #[prost(int32, tag = "2")]
    pub edit_access: i32,
}

/// Most pages a wiki lists or searches.
pub const MAX_PAGES: usize = 10_000;

const SIDEBAR: &str = "_Sidebar";
const FOOTER: &str = "_Footer";

const EXTENSIONS: &[(&str, WikiPageFormat)] = &[
    ("md", WikiPageFormat::Markdown),
    ("markdown", WikiPageFormat::Markdown),
    ("adoc", WikiPageFormat::Asciidoc),
    ("asciidoc", WikiPageFormat::Asciidoc),
    ("rst", WikiPageFormat::Rst),
    ("org", WikiPageFormat::Org),
    ("textile", WikiPageFormat::Textile),
    ("mediawiki", WikiPageFormat::Mediawiki),
];

fn ext_of(f: WikiPageFormat) -> &'static str {
    match f {
        WikiPageFormat::Asciidoc => "adoc",
        WikiPageFormat::Rst => "rst",
        WikiPageFormat::Org => "org",
        WikiPageFormat::Textile => "textile",
        WikiPageFormat::Mediawiki => "mediawiki",
        _ => "md",
    }
}

/// Splits a wiki file path into slug and format; `None` for files that are not pages.
pub fn page_of_path(path: &str) -> Option<(String, WikiPageFormat)> {
    if path.contains('/') {
        return None;
    }
    let (stem, ext) = path.rsplit_once('.')?;
    let (_, fmt) = EXTENSIONS
        .iter()
        .find(|(e, _)| e.eq_ignore_ascii_case(ext))?;
    (!stem.is_empty()).then(|| (stem.to_string(), *fmt))
}

/// The page slug of a title: spaces become `-`; `/`, control characters and leading dots are
/// refused.
pub fn slug_of_title(title: &str) -> Result<String> {
    let t = title.trim();
    required("title", t)?;
    max_len("title", t, MAX_TITLE)?;
    if t.chars().any(|c| c == '/' || c == '\\' || c.is_control()) || t.starts_with('.') {
        return Err(ForgeError::invalid(
            "invalid_title",
            "page titles cannot contain `/`, `\\` or control characters, or start with `.`",
        ));
    }
    let slug = t.split_whitespace().collect::<Vec<_>>().join("-");
    if slug == SIDEBAR || slug == FOOTER {
        return Err(ForgeError::invalid(
            "reserved_title",
            "`_Sidebar` and `_Footer` are special pages; use the sidebar and footer calls",
        ));
    }
    Ok(slug)
}

fn title_of_slug(slug: &str) -> String {
    slug.replace('-', " ")
}

fn escape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\`*_{}[]()#+-!<>|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Wikis over the VCS port.
pub struct Wiki {
    db: Db,
    vcs: Arc<dyn ContentVcs>,
    base_url: String,
}

impl Wiki {
    /// Wikis over `db` and `vcs`; `base_url` prefixes page links and clone URLs.
    pub fn new(db: Db, vcs: Arc<dyn ContentVcs>, base_url: &str) -> Wiki {
        Wiki {
            db,
            vcs,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn rec(&self, cx: &Cx, repo_id: &str) -> Result<(WikiRec, Option<Row>)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(match tx.get_msg::<WikiRec>(&WIKIS, "", repo_id).await? {
            Some((r, row)) => (r, Some(row)),
            None => (
                WikiRec {
                    enabled: false,
                    edit_access: WikiEditAccess::Collaborators as i32,
                },
                None,
            ),
        })
    }

    /// The wiki's edit policy and whether it is enabled.
    pub async fn settings(&self, cx: &Cx, repo: &Repo) -> Result<(bool, WikiEditAccess)> {
        let (r, _) = self.rec(cx, &repo.id).await?;
        Ok((
            r.enabled,
            WikiEditAccess::try_from(r.edit_access).unwrap_or(WikiEditAccess::Collaborators),
        ))
    }

    async fn require_enabled(&self, cx: &Cx, repo: &Repo) -> Result<WikiEditAccess> {
        let (enabled, access) = self.settings(cx, repo).await?;
        if enabled {
            Ok(access)
        } else {
            Err(ForgeError::invalid_state("the wiki is not enabled").with_subcode("wiki_disabled"))
        }
    }

    async fn info_of(&self, repo: &Repo, rec: &WikiRec) -> Result<WikiInfo> {
        let vr = VcsRepo::wiki_of(repo);
        let (head, pages) = if rec.enabled {
            let head = self.vcs.head(&vr).await?;
            let pages = match &head {
                Some(h) => self
                    .vcs
                    .tree(&vr, h)
                    .await?
                    .iter()
                    .filter(|f| {
                        page_of_path(&f.path).is_some_and(|(s, _)| s != SIDEBAR && s != FOOTER)
                    })
                    .count() as u32,
                None => 0,
            };
            (head, pages)
        } else {
            (None, 0)
        };
        Ok(WikiInfo {
            repo: super::common::repo_ref_id(&repo.id),
            enabled: rec.enabled,
            wiki_repo: Some(bee_proto::v1::RepoRef {
                repo: Some(bee_proto::v1::repo_ref::Repo::FullName(vr.name.clone())),
            }),
            default_ref: "main".into(),
            head,
            clone_url_https: format!("{}/{}.git", self.base_url, vr.name),
            clone_url_ssh: String::new(),
            edit_access: rec.edit_access,
            page_count: pages,
        })
    }

    /// Enables the wiki (creating its sibling repo) with an edit policy.
    pub async fn enable(&self, cx: &Cx, repo: &Repo, access: WikiEditAccess) -> Result<WikiInfo> {
        let access = if access == WikiEditAccess::Unspecified {
            WikiEditAccess::Collaborators
        } else {
            access
        };
        self.vcs.init(&VcsRepo::wiki_of(repo)).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rec = WikiRec {
            enabled: true,
            edit_access: access as i32,
        };
        tx.put(&WIKIS, &Row::new("", repo.id.clone()).msg(&rec))
            .await?;
        let payload = serde_json::json!({"edit_access": access.as_str_name(), "wiki_repo": format!("{}.wiki", repo.full_name)});
        emit(
            &mut tx,
            "wiki.enabled",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "wiki.enable",
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        self.info_of(repo, &rec).await
    }

    /// Disables the wiki; its repo and history are kept.
    pub async fn disable(&self, cx: &Cx, repo: &Repo) -> Result<WikiInfo> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = match tx.get_msg::<WikiRec>(&WIKIS, "", &repo.id).await? {
            Some(x) => x,
            None => {
                return Err(ForgeError::invalid_state("the wiki is not enabled")
                    .with_subcode("wiki_disabled"));
            }
        };
        if !rec.enabled {
            return Err(
                ForgeError::invalid_state("the wiki is not enabled").with_subcode("wiki_disabled")
            );
        }
        rec.enabled = false;
        tx.update(&WIKIS, &row.msg(&rec)).await?;
        let payload = serde_json::json!({"wiki_repo": format!("{}.wiki", repo.full_name)});
        emit(
            &mut tx,
            "wiki.disabled",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "wiki.disable",
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        self.info_of(repo, &rec).await
    }

    /// The wiki's info.
    pub async fn info(&self, cx: &Cx, repo: &Repo) -> Result<WikiInfo> {
        let (rec, _) = self.rec(cx, &repo.id).await?;
        self.info_of(repo, &rec).await
    }

    async fn revision(&self, vr: &VcsRepo, at: &str) -> Result<Option<ObjectId>> {
        if at.is_empty() {
            return self.vcs.head(vr).await;
        }
        self.vcs.resolve(vr, at).await.map(Some)
    }

    async fn files(&self, vr: &VcsRepo, at: &Option<ObjectId>) -> Result<Vec<FileEntry>> {
        match at {
            Some(h) => self.vcs.tree(vr, h).await,
            None => Ok(Vec::new()),
        }
    }

    fn find<'a>(files: &'a [FileEntry], slug: &str) -> Option<(&'a FileEntry, WikiPageFormat)> {
        files.iter().find_map(|f| {
            let (s, fmt) = page_of_path(&f.path)?;
            (s == slug).then_some((f, fmt))
        })
    }

    fn summary(
        f: &FileEntry,
        slug: &str,
        fmt: WikiPageFormat,
        last: Option<&CommitInfo>,
    ) -> WikiPageSummary {
        WikiPageSummary {
            slug: slug.to_string(),
            title: title_of_slug(slug),
            path: f.path.clone().into_bytes(),
            format: fmt as i32,
            size: f.size,
            updated_at: last.map(|c| ts(c.time_ms)),
        }
    }

    /// Pages at a revision (default head), sorted by slug.
    pub async fn list_pages(
        &self,
        cx: &Cx,
        repo: &Repo,
        at: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<WikiPageSummary>, String)> {
        self.require_enabled(cx, repo).await?;
        let vr = VcsRepo::wiki_of(repo);
        let rev = self.revision(&vr, at).await?;
        let mut pages: Vec<(FileEntry, String, WikiPageFormat)> = self
            .files(&vr, &rev)
            .await?
            .into_iter()
            .filter_map(|f| {
                let (s, fmt) = page_of_path(&f.path)?;
                (s != SIDEBAR && s != FOOTER).then_some((f, s, fmt))
            })
            .collect();
        pages.sort_by(|a, b| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then(a.1.cmp(&b.1))
        });
        pages.truncate(MAX_PAGES);
        let (page, next) = page_vec(pages, token, limit)?;
        let mut out = Vec::new();
        for (f, slug, fmt) in page {
            let last = match &rev {
                Some(h) => self
                    .vcs
                    .log(&vr, h, Some(&f.path), 1)
                    .await?
                    .into_iter()
                    .next(),
                None => None,
            };
            out.push(Self::summary(&f, &slug, fmt, last.as_ref()));
        }
        Ok((out, next))
    }

    async fn read_page(
        &self,
        repo: &Repo,
        vr: &VcsRepo,
        rev: &ObjectId,
        f: &FileEntry,
        slug: &str,
        fmt: WikiPageFormat,
    ) -> Result<WikiPage> {
        let content = String::from_utf8_lossy(&self.vcs.read(vr, &f.blob).await?).into_owned();
        let last = self
            .vcs
            .log(vr, rev, Some(&f.path), 1)
            .await?
            .into_iter()
            .next();
        Ok(WikiPage {
            slug: slug.to_string(),
            title: title_of_slug(slug),
            path: f.path.clone().into_bytes(),
            format: fmt as i32,
            content,
            content_html: String::new(),
            changeset: last.as_ref().map(|c| c.id.clone()),
            blob: Some(f.blob.clone()),
            last_editor: last.as_ref().map(|c| c.author.clone()),
            updated_at: last.as_ref().map(|c| ts(c.time_ms)),
            url: format!("{}/{}/wiki/{slug}", self.base_url, repo.full_name),
        })
    }

    /// One page at a revision (default head).
    pub async fn get_page(&self, cx: &Cx, repo: &Repo, slug: &str, at: &str) -> Result<WikiPage> {
        self.require_enabled(cx, repo).await?;
        required("slug", slug)?;
        let vr = VcsRepo::wiki_of(repo);
        let rev = self.revision(&vr, at).await?;
        let files = self.files(&vr, &rev).await?;
        match (Self::find(&files, slug), &rev) {
            (Some((f, fmt)), Some(h)) => self.read_page(repo, &vr, h, f, slug, fmt).await,
            _ => Err(ForgeError::not_found(format!("wiki page {slug}"))),
        }
    }

    async fn commit(
        &self,
        cx: &Cx,
        repo: &Repo,
        vr: &VcsRepo,
        head: Option<ObjectId>,
        changes: Vec<FileChange>,
        message: String,
    ) -> Result<CommitInfo> {
        let time_ms = self.db.now_ms();
        self.vcs
            .commit(
                vr,
                CommitRequest {
                    expected_head: head,
                    changes,
                    author: cx.principal(),
                    message,
                    time_ms,
                },
            )
            .await
            .map_err(|e| {
                if e.reason == bee_proto::v1::ErrorReason::VersionMismatch {
                    ForgeError::new(
                        bee_proto::v1::ErrorReason::VersionMismatch,
                        format!(
                            "the wiki of {} changed while you were editing; reload and retry",
                            repo.full_name
                        ),
                    )
                    .with_subcode("wiki_head_moved")
                } else {
                    e
                }
            })
    }

    async fn record(
        &self,
        cx: &Cx,
        repo: &Repo,
        action: &str,
        slug: &str,
        c: &CommitInfo,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let payload = serde_json::json!({
            "action": action,
            "pages": [{"slug": slug, "title": title_of_slug(slug), "action": action, "changeset": hex(&c.id.raw)}],
            "wiki_repo": format!("{}.wiki", repo.full_name),
        });
        emit(
            &mut tx,
            "wiki.page_updated",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::WikiPage, &repo.id, slug),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            &format!("wiki.page_{action}"),
            &repo.org_id,
            &repo.id,
            SubjectKind::WikiPage,
            slug,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    /// Creates a page.
    pub async fn create_page(
        &self,
        cx: &Cx,
        repo: &Repo,
        title: &str,
        content: &str,
        format: WikiPageFormat,
        message: &str,
    ) -> Result<WikiPage> {
        self.require_enabled(cx, repo).await?;
        let slug = slug_of_title(title)?;
        max_len("content", content, MAX_BODY)?;
        let fmt = if format == WikiPageFormat::Unspecified {
            WikiPageFormat::Markdown
        } else {
            format
        };
        let vr = VcsRepo::wiki_of(repo);
        let head = self.vcs.head(&vr).await?;
        let files = self.files(&vr, &head).await?;
        if Self::find(&files, &slug).is_some() {
            return Err(ForgeError::already_exists(format!("wiki page {slug}")));
        }
        let path = format!("{slug}.{}", ext_of(fmt));
        let msg = if message.trim().is_empty() {
            format!("Create {}", title.trim())
        } else {
            message.to_string()
        };
        let c = self
            .commit(
                cx,
                repo,
                &vr,
                head,
                vec![FileChange {
                    path,
                    content: Some(content.as_bytes().to_vec()),
                }],
                msg,
            )
            .await?;
        self.record(cx, repo, "created", &slug, &c).await?;
        self.get_page(cx, repo, &slug, "").await
    }

    /// Edits a page, optionally renaming it (`title`) or changing its format. With
    /// `expected_changeset` set, a page edited since is `VERSION_MISMATCH`.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_page(
        &self,
        cx: &Cx,
        repo: &Repo,
        slug: &str,
        expected: Option<&ObjectId>,
        title: Option<&str>,
        content: Option<&str>,
        format: Option<WikiPageFormat>,
        message: &str,
    ) -> Result<WikiPage> {
        self.require_enabled(cx, repo).await?;
        let vr = VcsRepo::wiki_of(repo);
        let head = self.vcs.head(&vr).await?;
        let files = self.files(&vr, &head).await?;
        let (file, cur_fmt) = Self::find(&files, slug)
            .ok_or_else(|| ForgeError::not_found(format!("wiki page {slug}")))?;
        let h = head.clone().ok_or_else(|| ForgeError::not_found("wiki"))?;
        let last = self
            .vcs
            .log(&vr, &h, Some(&file.path), 1)
            .await?
            .into_iter()
            .next();
        if let (Some(want), Some(have)) = (expected, last.as_ref())
            && !want.raw.is_empty()
            && want.raw != have.id.raw
        {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::VersionMismatch,
                format!("page {slug} changed since changeset {}", hex(&want.raw)),
            )
            .with_subcode("page_changed"));
        }
        let new_slug = match title {
            Some(t) => slug_of_title(t)?,
            None => slug.to_string(),
        };
        if new_slug != slug && Self::find(&files, &new_slug).is_some() {
            return Err(ForgeError::already_exists(format!("wiki page {new_slug}")));
        }
        let fmt = format
            .filter(|f| *f != WikiPageFormat::Unspecified)
            .unwrap_or(cur_fmt);
        let new_path = format!("{new_slug}.{}", ext_of(fmt));
        let body = match content {
            Some(c) => {
                max_len("content", c, MAX_BODY)?;
                c.as_bytes().to_vec()
            }
            None => self.vcs.read(&vr, &file.blob).await?,
        };
        let mut changes = Vec::new();
        if new_path != file.path {
            changes.push(FileChange {
                path: file.path.clone(),
                content: None,
            });
        }
        changes.push(FileChange {
            path: new_path,
            content: Some(body),
        });
        let display = title.map(str::trim).unwrap_or(slug);
        let msg = if message.trim().is_empty() {
            format!("Update {display}")
        } else {
            message.to_string()
        };
        let c = self.commit(cx, repo, &vr, head, changes, msg).await?;
        self.record(
            cx,
            repo,
            if new_slug != slug {
                "renamed"
            } else {
                "edited"
            },
            &new_slug,
            &c,
        )
        .await?;
        self.get_page(cx, repo, &new_slug, "").await
    }

    /// Deletes a page; returns the changeset that removed it.
    pub async fn delete_page(
        &self,
        cx: &Cx,
        repo: &Repo,
        slug: &str,
        message: &str,
    ) -> Result<ObjectId> {
        self.require_enabled(cx, repo).await?;
        let vr = VcsRepo::wiki_of(repo);
        let head = self.vcs.head(&vr).await?;
        let files = self.files(&vr, &head).await?;
        let (file, _) = Self::find(&files, slug)
            .ok_or_else(|| ForgeError::not_found(format!("wiki page {slug}")))?;
        let msg = if message.trim().is_empty() {
            format!("Delete {}", title_of_slug(slug))
        } else {
            message.to_string()
        };
        let c = self
            .commit(
                cx,
                repo,
                &vr,
                head,
                vec![FileChange {
                    path: file.path.clone(),
                    content: None,
                }],
                msg,
            )
            .await?;
        self.record(cx, repo, "deleted", slug, &c).await?;
        Ok(c.id)
    }

    /// The history of a page, newest first.
    pub async fn revisions(
        &self,
        cx: &Cx,
        repo: &Repo,
        slug: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<WikiPageRevision>, String)> {
        self.require_enabled(cx, repo).await?;
        let vr = VcsRepo::wiki_of(repo);
        let head = self.vcs.head(&vr).await?;
        let files = self.files(&vr, &head).await?;
        let (file, _) = Self::find(&files, slug)
            .ok_or_else(|| ForgeError::not_found(format!("wiki page {slug}")))?;
        let h = head.ok_or_else(|| ForgeError::not_found("wiki"))?;
        let log = self.vcs.log(&vr, &h, Some(&file.path), MAX_PAGES).await?;
        let revs: Vec<WikiPageRevision> = log
            .into_iter()
            .map(|c| WikiPageRevision {
                changeset: Some(c.id),
                author: Some(c.author),
                message: c.message,
                created_at: Some(ts(c.time_ms)),
                path: file.path.clone().into_bytes(),
            })
            .collect();
        page_vec(revs, token, limit)
    }

    async fn special(&self, cx: &Cx, repo: &Repo, name: &str, at: &str) -> Result<WikiPage> {
        self.get_page_special(cx, repo, name, at).await
    }

    async fn get_page_special(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
        at: &str,
    ) -> Result<WikiPage> {
        self.require_enabled(cx, repo).await?;
        let vr = VcsRepo::wiki_of(repo);
        let rev = self.revision(&vr, at).await?;
        let files = self.files(&vr, &rev).await?;
        match (Self::find(&files, name), &rev) {
            (Some((f, fmt)), Some(h)) => self.read_page(repo, &vr, h, f, name, fmt).await,
            _ => Err(ForgeError::not_found(format!(
                "wiki {}",
                name.trim_start_matches('_').to_lowercase()
            ))),
        }
    }

    /// The sidebar page.
    pub async fn sidebar(&self, cx: &Cx, repo: &Repo, at: &str) -> Result<WikiPage> {
        self.special(cx, repo, SIDEBAR, at).await
    }

    /// The footer page.
    pub async fn footer(&self, cx: &Cx, repo: &Repo, at: &str) -> Result<WikiPage> {
        self.special(cx, repo, FOOTER, at).await
    }

    /// Sets the sidebar (empty content removes it).
    pub async fn set_sidebar(
        &self,
        cx: &Cx,
        repo: &Repo,
        content: &str,
        message: &str,
    ) -> Result<WikiPage> {
        self.set_special(cx, repo, SIDEBAR, content, message).await
    }

    /// Sets the footer (empty content removes it).
    pub async fn set_footer(
        &self,
        cx: &Cx,
        repo: &Repo,
        content: &str,
        message: &str,
    ) -> Result<WikiPage> {
        self.set_special(cx, repo, FOOTER, content, message).await
    }

    async fn set_special(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
        content: &str,
        message: &str,
    ) -> Result<WikiPage> {
        self.require_enabled(cx, repo).await?;
        max_len("content", content, MAX_BODY)?;
        let vr = VcsRepo::wiki_of(repo);
        let head = self.vcs.head(&vr).await?;
        let files = self.files(&vr, &head).await?;
        let existing = Self::find(&files, name).map(|(f, _)| f.path.clone());
        let path = existing.clone().unwrap_or_else(|| format!("{name}.md"));
        let (change, action) = if content.is_empty() {
            if existing.is_none() {
                return Err(ForgeError::not_found(format!(
                    "wiki {}",
                    name.trim_start_matches('_').to_lowercase()
                )));
            }
            (
                FileChange {
                    path,
                    content: None,
                },
                "deleted",
            )
        } else {
            (
                FileChange {
                    path,
                    content: Some(content.as_bytes().to_vec()),
                },
                if existing.is_some() {
                    "edited"
                } else {
                    "created"
                },
            )
        };
        let msg = if message.trim().is_empty() {
            format!("Update {}", name.trim_start_matches('_').to_lowercase())
        } else {
            message.to_string()
        };
        let c = self.commit(cx, repo, &vr, head, vec![change], msg).await?;
        self.record(cx, repo, action, name, &c).await?;
        if content.is_empty() {
            Ok(WikiPage {
                slug: name.to_string(),
                changeset: Some(c.id),
                ..Default::default()
            })
        } else {
            self.get_page_special(cx, repo, name, "").await
        }
    }

    /// Searches page titles and content (case-insensitive substring, every term must match);
    /// hits are ordered by score, then slug.
    pub async fn search(
        &self,
        cx: &Cx,
        repo: &Repo,
        query: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<WikiSearchHit>, String)> {
        self.require_enabled(cx, repo).await?;
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() {
            return Err(ForgeError::missing("query"));
        }
        let vr = VcsRepo::wiki_of(repo);
        let head = self.vcs.head(&vr).await?;
        let files = self.files(&vr, &head).await?;
        let mut hits: Vec<WikiSearchHit> = Vec::new();
        for f in files.iter().take(MAX_PAGES) {
            let Some((slug, fmt)) = page_of_path(&f.path) else {
                continue;
            };
            if slug == SIDEBAR || slug == FOOTER {
                continue;
            }
            let text = String::from_utf8_lossy(&self.vcs.read(&vr, &f.blob).await?).into_owned();
            let lower = text.to_lowercase();
            let title = title_of_slug(&slug).to_lowercase();
            if !terms
                .iter()
                .all(|t| lower.contains(t.as_str()) || title.contains(t.as_str()))
            {
                continue;
            }
            let score: usize = terms
                .iter()
                .map(|t| lower.matches(t.as_str()).count() + 5 * title.matches(t.as_str()).count())
                .sum();
            let snippets: Vec<String> = text
                .lines()
                .filter(|l| {
                    let l = l.to_lowercase();
                    terms.iter().any(|t| l.contains(t.as_str()))
                })
                .take(3)
                .map(|l| escape_md(&l.trim().chars().take(200).collect::<String>()))
                .collect();
            hits.push(WikiSearchHit {
                page: Some(Self::summary(f, &slug, fmt, None)),
                snippets,
                score: score as f32,
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    a.page
                        .as_ref()
                        .map(|p| &p.slug)
                        .cmp(&b.page.as_ref().map(|p| &p.slug))
                })
        });
        page_vec(hits, token, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_paths() {
        assert_eq!(
            slug_of_title("Getting  Started").unwrap(),
            "Getting-Started"
        );
        assert!(slug_of_title("a/b").is_err());
        assert!(slug_of_title("_Sidebar").is_err());
        assert_eq!(
            page_of_path("Home.md").map(|(s, f)| (s, f as i32)),
            Some(("Home".to_string(), WikiPageFormat::Markdown as i32))
        );
        assert!(page_of_path("img/x.png").is_none());
        assert!(page_of_path("notes.txt").is_none());
        assert_eq!(escape_md("a*b"), "a\\*b");
    }
}
