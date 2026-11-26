//! Gists (PRODUCT §4.16, F416-034): a lightweight user-owned repo kind. Each gist is a repo
//! reached through the [`ContentVcs`](super::ports::ContentVcs) port (`gist/<id>`), so files
//! have revisions and the gist is cloneable; the forge keeps metadata, stars, forks and
//! comments.
//!
//! # Visibility
//!
//! A gist is public (listed) or secret (reachable by anyone who has its id, never listed).
//! Visibility is fixed at creation. Rows live in scope `pub` or `sec`, so a read tries both.
//!
//! | table | scope | id | keys |
//! |---|---|---|---|
//! | gists | `pub` / `sec` | gist ULID | `k1` owner, `k2` fork parent, `n1` updated |
//! | stars | gist id | principal | `k1` principal, `n1` starred at |
//! | comments | gist id | ULID | `n1` created |
//!
//! Counters (stars, forks, comments) are denormalized into the gist record and change in the
//! transaction that changes what they count.

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_proto::v1::{Comment, Gist, GistFile, GistRevision, ObjectId, Principal, SubjectKind};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::module::EventKind;
use crate::store::{Db, Index, Key, Row, Scan, TableDef, Tx};

use super::SURFACE;
use super::common::{
    MAX_BODY, MAX_SHORT, draft, emit, max_len, page_vec, paged, required, subject,
};
use super::lang::{content_type_of, language_of};
use super::ports::{CommitRequest, ContentVcs, FileChange, FileEntry, UrlSigner, VcsRepo};

/// Gists.
pub static GISTS: TableDef = TableDef {
    name: "forge_gists",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::ScopeN1],
};
/// Gist stars.
pub static STARS: TableDef = TableDef {
    name: "forge_gist_stars",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::K1, Index::N1],
};
/// Gist comments.
pub static COMMENTS: TableDef = TableDef {
    name: "forge_gist_comments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};

/// Gist event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::new("gist.created", "A gist was created"),
    EventKind::new("gist.updated", "A gist was updated"),
    EventKind::new("gist.deleted", "A gist was deleted"),
    EventKind::new("gist.forked", "A gist was forked"),
    EventKind::new("gist.starred", "A gist was starred"),
    EventKind::new("gist.unstarred", "A gist was unstarred"),
    EventKind::new("gist.commented", "A comment was added to a gist"),
];

/// Most files in a gist.
pub const MAX_FILES: usize = 300;
/// Default per-file inline limit.
pub const DEFAULT_INLINE: usize = 64 * 1024;

/// A gist's stored metadata.
#[derive(Clone, PartialEq, Message)]
pub struct GistRec {
    /// The gist without files; counters denormalized.
    #[prost(message, optional, tag = "1")]
    pub gist: Option<Gist>,
}

/// A new gist.
#[derive(Clone, Debug, Default)]
pub struct NewGist {
    /// Filename and content pairs.
    pub files: Vec<(String, String)>,
    /// Description.
    pub description: String,
    /// Public (listed) or secret.
    pub public: bool,
}

/// One edit of a gist's files.
#[derive(Clone, Debug, Default)]
pub struct FileEdit {
    /// The existing or new file name.
    pub filename: String,
    /// New content; `None` keeps it.
    pub content: Option<String>,
    /// Rename to.
    pub new_filename: Option<String>,
    /// Delete the file.
    pub delete: bool,
}

/// Which gists a list returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    /// The caller's, public and secret.
    Mine,
    /// A user's public gists.
    User,
    /// Every public gist.
    Public,
    /// Gists the caller starred.
    Starred,
}

fn valid_filename(name: &str) -> Result<()> {
    required("filename", name)?;
    max_len("filename", name, 255)?;
    if name.contains('/')
        || name.contains('\\')
        || name.chars().any(|c| c.is_control())
        || name == "."
        || name == ".."
    {
        return Err(ForgeError::invalid(
            "invalid_filename",
            format!("{name:?} is not a valid gist file name"),
        ));
    }
    Ok(())
}

/// Gists over the store and the VCS port.
pub struct Gists {
    db: Db,
    vcs: Arc<dyn ContentVcs>,
    signer: Arc<dyn UrlSigner>,
    base_url: String,
}

impl Gists {
    /// Gists over `db`, `vcs` and `signer`.
    pub fn new(
        db: Db,
        vcs: Arc<dyn ContentVcs>,
        signer: Arc<dyn UrlSigner>,
        base_url: &str,
    ) -> Gists {
        Gists {
            db,
            vcs,
            signer,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn load(tx: &mut Tx, id: &str) -> Result<(GistRec, Row)> {
        for scope in ["pub", "sec"] {
            if let Some(x) = tx.get_msg::<GistRec>(&GISTS, scope, id).await? {
                return Ok(x);
            }
        }
        Err(ForgeError::not_found(format!("gist {id}")))
    }

    async fn load_locked(tx: &mut Tx, id: &str) -> Result<(GistRec, Row)> {
        for scope in ["pub", "sec"] {
            if let Some(row) = tx.get_for_update(&GISTS, scope, id).await? {
                return Ok((row.decode()?, row));
            }
        }
        Err(ForgeError::not_found(format!("gist {id}")))
    }

    fn files_view(
        &self,
        entries: &[FileEntry],
        contents: &BTreeMap<String, Vec<u8>>,
        inline_max: usize,
    ) -> Vec<GistFile> {
        let mut files: Vec<GistFile> = entries
            .iter()
            .map(|e| {
                let bytes = contents.get(&e.path);
                let binary = bytes.is_some_and(|b| std::str::from_utf8(b).is_err());
                let (content, truncated) = match bytes {
                    Some(b) if !binary => {
                        if b.len() > inline_max {
                            let mut cut = inline_max;
                            while cut > 0 && std::str::from_utf8(&b[..cut]).is_err() {
                                cut -= 1;
                            }
                            (String::from_utf8_lossy(&b[..cut]).into_owned(), true)
                        } else {
                            (String::from_utf8_lossy(b).into_owned(), false)
                        }
                    }
                    _ => (String::new(), false),
                };
                GistFile {
                    filename: e.path.clone(),
                    language: language_of(&e.path)
                        .map(|l| l.0.to_string())
                        .unwrap_or_default(),
                    content_type: if binary {
                        "application/octet-stream".into()
                    } else {
                        content_type_of(&e.path).into()
                    },
                    size: e.size,
                    blob: Some(e.blob.clone()),
                    content,
                    truncated,
                    binary,
                }
            })
            .collect();
        files.sort_by(|a, b| a.filename.cmp(&b.filename));
        files
    }

    async fn view(&self, mut g: Gist, rev: Option<&ObjectId>, inline_max: usize) -> Result<Gist> {
        let vr = VcsRepo::gist(&g.id);
        let at = match rev {
            Some(r) => Some(r.clone()),
            None => self.vcs.head(&vr).await?,
        };
        let mut contents = BTreeMap::new();
        let entries = match &at {
            Some(h) => self.vcs.tree(&vr, h).await?,
            None => Vec::new(),
        };
        if inline_max > 0 {
            for e in &entries {
                contents.insert(e.path.clone(), self.vcs.read(&vr, &e.blob).await?);
            }
        }
        g.files = self.files_view(&entries, &contents, inline_max);
        g.head = at;
        g.url = format!("{}/gists/{}", self.base_url, g.id);
        g.clone_url_https = format!("{}/gists/{}.git", self.base_url, g.id);
        g.embed_url = format!("{}/gists/{}.js", self.base_url, g.id);
        g.repo = super::common::repo_ref_id(&g.id).map(|_| bee_proto::v1::RepoRef {
            repo: Some(bee_proto::v1::repo_ref::Repo::FullName(vr.name.clone())),
        });
        Ok(g)
    }

    async fn emit_gist(
        tx: &mut Tx,
        kind: &str,
        audit: &str,
        g: &Gist,
        extra: serde_json::Value,
    ) -> Result<()> {
        let payload = serde_json::json!({
            "gist_id": g.id,
            "owner": g.owner,
            "public": g.public,
            "description": g.description,
            "detail": extra,
        });
        emit(
            tx,
            kind,
            "",
            "",
            subject(SubjectKind::Gist, "", &g.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(audit, "", "", SubjectKind::Gist, &g.id, &payload))
            .await
    }

    /// Creates a gist with its files as the first changeset.
    pub async fn create(&self, cx: &Cx, n: NewGist) -> Result<Gist> {
        if n.files.is_empty() {
            return Err(ForgeError::missing("files"));
        }
        if n.files.len() > MAX_FILES {
            return Err(ForgeError::too_large(
                "gist_files",
                MAX_FILES as u64,
                n.files.len() as u64,
            ));
        }
        max_len("description", &n.description, MAX_SHORT)?;
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        for (name, content) in n.files {
            valid_filename(&name)?;
            max_len("content", &content, MAX_BODY)?;
            if content.is_empty() {
                return Err(ForgeError::invalid(
                    "empty_file",
                    format!("file {name:?} is empty; gist files need content"),
                ));
            }
            files.insert(name, content);
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = tx.new_id()?;
        let now = tx.now_ms();
        let vr = VcsRepo::gist(&id);
        self.vcs.init(&vr).await?;
        self.vcs
            .commit(
                &vr,
                CommitRequest {
                    expected_head: None,
                    changes: files
                        .iter()
                        .map(|(p, c)| FileChange {
                            path: p.clone(),
                            content: Some(c.as_bytes().to_vec()),
                        })
                        .collect(),
                    author: cx.principal(),
                    message: "Create gist".into(),
                    time_ms: now,
                },
            )
            .await?;
        let g = Gist {
            id: id.clone(),
            owner: Some(cx.principal()),
            description: n.description,
            public: n.public,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        let scope = if g.public { "pub" } else { "sec" };
        let row = Row::new(scope, id)
            .k1(cx.actor_id.clone())
            .n1(now as i64)
            .msg(&GistRec {
                gist: Some(g.clone()),
            });
        tx.insert(&GISTS, &row).await?;
        Self::emit_gist(
            &mut tx,
            "gist.created",
            "gist.create",
            &g,
            serde_json::json!({"files": files.keys().collect::<Vec<_>>()}),
        )
        .await?;
        tx.commit().await?;
        self.view(g, None, DEFAULT_INLINE).await
    }

    /// A gist at its head or at a revision. `inline_max` bounds per-file content (0 omits it).
    pub async fn get(
        &self,
        cx: &Cx,
        id: &str,
        rev: Option<&ObjectId>,
        inline_max: usize,
    ) -> Result<Gist> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load(&mut tx, id).await?;
        drop(tx);
        let mut g = rec.gist.unwrap_or_default();
        g.updated_at = row.n1.map(|n| ts(n as u64)).or(g.updated_at);
        self.view(g, rev, inline_max).await
    }

    /// Gists of a kind, newest update first, without file content.
    pub async fn list(
        &self,
        cx: &Cx,
        kind: ListKind,
        user_id: &str,
        since_ms: u64,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Gist>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut recs: Vec<(Gist, i64)> = Vec::new();
        match kind {
            ListKind::Public => {
                let (rows, next) = paged(
                    &mut tx,
                    &GISTS,
                    Scan::scope("pub", limit)
                        .by_n1()
                        .desc()
                        .n1_min(since_ms as i64),
                    token,
                    limit,
                )
                .await?;
                for r in rows {
                    recs.push((
                        r.decode::<GistRec>()?.gist.unwrap_or_default(),
                        r.n1.unwrap_or(0),
                    ));
                }
                drop(tx);
                return Ok((self.views(recs).await?, next));
            }
            ListKind::Mine | ListKind::User => {
                let owner = if kind == ListKind::Mine {
                    cx.actor_id.clone()
                } else {
                    user_id.to_string()
                };
                let scopes: &[&str] = if kind == ListKind::Mine {
                    &["pub", "sec"]
                } else {
                    &["pub"]
                };
                for scope in scopes {
                    let rows = tx
                        .scan(
                            &GISTS,
                            &Scan::scope(*scope, 5000)
                                .key(Key::K1(owner.clone()))
                                .by_n1()
                                .desc()
                                .n1_min(since_ms as i64),
                        )
                        .await?;
                    for r in rows {
                        recs.push((
                            r.decode::<GistRec>()?.gist.unwrap_or_default(),
                            r.n1.unwrap_or(0),
                        ));
                    }
                }
            }
            ListKind::Starred => {
                let stars = tx
                    .scan(
                        &STARS,
                        &Scan {
                            scope: None,
                            key: Some(Key::K1(cx.actor_id.clone())),
                            order: crate::store::Order::N1,
                            descending: true,
                            limit: 5000,
                            ..Scan::default()
                        },
                    )
                    .await?;
                for s in stars {
                    if let Ok((rec, row)) = Self::load(&mut tx, &s.scope).await {
                        recs.push((rec.gist.unwrap_or_default(), row.n1.unwrap_or(0)));
                    }
                }
            }
        }
        recs.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.id.cmp(&a.0.id)));
        let (page, next) = page_vec(recs, token, limit)?;
        drop(tx);
        Ok((self.views(page).await?, next))
    }

    async fn views(&self, recs: Vec<(Gist, i64)>) -> Result<Vec<Gist>> {
        let mut out = Vec::new();
        for (mut g, n1) in recs {
            g.updated_at = Some(ts(n1.max(0) as u64));
            out.push(self.view(g, None, 0).await?);
        }
        Ok(out)
    }

    fn owner_only(g: &Gist, cx: &Cx) -> Result<()> {
        if g.owner.as_ref().map(|o| o.id.as_str()) == Some(cx.actor_id.as_str()) {
            Ok(())
        } else {
            Err(ForgeError::permission_denied(
                "only the owner may change this gist",
            ))
        }
    }

    /// Edits a gist: description, files (write, rename, delete) and comment lock. File edits
    /// are one changeset; with `expected_head` set, a gist that moved is `VERSION_MISMATCH`.
    pub async fn update(
        &self,
        cx: &Cx,
        id: &str,
        expected_head: Option<&ObjectId>,
        description: Option<&str>,
        edits: &[FileEdit],
        comments_locked: Option<bool>,
    ) -> Result<Gist> {
        if let Some(d) = description {
            max_len("description", d, MAX_SHORT)?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, id).await?;
        let mut g = rec.gist.clone().unwrap_or_default();
        Self::owner_only(&g, cx)?;
        let now = tx.now_ms();
        let vr = VcsRepo::gist(id);
        let head = self.vcs.head(&vr).await?;
        if let Some(want) = expected_head
            && !want.raw.is_empty()
            && head.as_ref().map(|h| &h.raw) != Some(&want.raw)
        {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::VersionMismatch,
                "the gist changed since you read it",
            )
            .with_subcode("gist_head_moved"));
        }
        let mut changed_files = false;
        if !edits.is_empty() {
            let h = head
                .clone()
                .ok_or_else(|| ForgeError::not_found("gist content"))?;
            let current = self.vcs.tree(&vr, &h).await?;
            let mut names: Vec<String> = current.iter().map(|f| f.path.clone()).collect();
            let mut changes: Vec<FileChange> = Vec::new();
            for e in edits {
                valid_filename(&e.filename)?;
                let exists = names.contains(&e.filename);
                if e.delete {
                    if !exists {
                        return Err(ForgeError::not_found(format!("gist file {}", e.filename)));
                    }
                    names.retain(|n| *n != e.filename);
                    changes.push(FileChange {
                        path: e.filename.clone(),
                        content: None,
                    });
                    continue;
                }
                let mut content: Option<Vec<u8>> = None;
                if let Some(c) = &e.content {
                    max_len("content", c, MAX_BODY)?;
                    if c.is_empty() {
                        return Err(ForgeError::invalid("empty_file", "gist files need content"));
                    }
                    content = Some(c.as_bytes().to_vec());
                }
                if !exists && content.is_none() {
                    return Err(ForgeError::not_found(format!("gist file {}", e.filename)));
                }
                let target = e.new_filename.clone().unwrap_or_else(|| e.filename.clone());
                valid_filename(&target)?;
                if target != e.filename {
                    if names.contains(&target) {
                        return Err(ForgeError::already_exists(format!("gist file {target}")));
                    }
                    let body =
                        match content {
                            Some(c) => c,
                            None => {
                                let f = current.iter().find(|f| f.path == e.filename).ok_or_else(
                                    || ForgeError::not_found(format!("gist file {}", e.filename)),
                                )?;
                                self.vcs.read(&vr, &f.blob).await?
                            }
                        };
                    names.retain(|n| *n != e.filename);
                    names.push(target.clone());
                    changes.push(FileChange {
                        path: e.filename.clone(),
                        content: None,
                    });
                    changes.push(FileChange {
                        path: target,
                        content: Some(body),
                    });
                } else if let Some(c) = content {
                    if !exists {
                        names.push(target.clone());
                    }
                    changes.push(FileChange {
                        path: target,
                        content: Some(c),
                    });
                }
            }
            if names.is_empty() {
                return Err(ForgeError::invalid(
                    "gist_empty",
                    "a gist needs at least one file",
                ));
            }
            if names.len() > MAX_FILES {
                return Err(ForgeError::too_large(
                    "gist_files",
                    MAX_FILES as u64,
                    names.len() as u64,
                ));
            }
            if !changes.is_empty() {
                self.vcs
                    .commit(
                        &vr,
                        CommitRequest {
                            expected_head: head,
                            changes,
                            author: cx.principal(),
                            message: "Update gist".into(),
                            time_ms: now,
                        },
                    )
                    .await?;
                changed_files = true;
            }
        }
        if let Some(d) = description {
            g.description = d.to_string();
        }
        if let Some(l) = comments_locked {
            g.comments_locked = l;
        }
        g.updated_at = Some(ts(now));
        rec.gist = Some(g.clone());
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        tx.update(&GISTS, &row).await?;
        Self::emit_gist(&mut tx, "gist.updated", "gist.update", &g, serde_json::json!({"files_changed": changed_files, "comments_locked": g.comments_locked})).await?;
        tx.commit().await?;
        self.view(g, None, DEFAULT_INLINE).await
    }

    /// Deletes a gist with its history, stars and comments (owner only).
    pub async fn delete(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load_locked(&mut tx, id).await?;
        let g = rec.gist.unwrap_or_default();
        Self::owner_only(&g, cx)?;
        for r in tx
            .scan(&STARS, &Scan::scope(id, crate::store::MAX_SCAN))
            .await?
        {
            tx.delete(&STARS, id, &r.id).await?;
        }
        for r in tx
            .scan(&COMMENTS, &Scan::scope(id, crate::store::MAX_SCAN))
            .await?
        {
            tx.delete(&COMMENTS, id, &r.id).await?;
        }
        tx.delete(&GISTS, &row.scope, id).await?;
        if let Some(parent) = row.k2.clone() {
            // Keep the parent's fork counter exact.
            if let Ok((mut prec, mut prow)) = Self::load_locked(&mut tx, &parent).await {
                let mut pg = prec.gist.clone().unwrap_or_default();
                pg.fork_count = pg.fork_count.saturating_sub(1);
                prec.gist = Some(pg);
                prow.body = prec.encode_to_vec();
                tx.update(&GISTS, &prow).await?;
            }
        }
        Self::emit_gist(
            &mut tx,
            "gist.deleted",
            "gist.delete",
            &g,
            serde_json::Value::Null,
        )
        .await?;
        tx.commit().await?;
        self.vcs.remove(&VcsRepo::gist(id)).await
    }

    /// Revisions, newest first.
    pub async fn revisions(
        &self,
        cx: &Cx,
        id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<GistRevision>, String)> {
        let g = self.get(cx, id, None, 0).await?;
        let vr = VcsRepo::gist(id);
        let head = g
            .head
            .ok_or_else(|| ForgeError::not_found("gist content"))?;
        let log = self.vcs.log(&vr, &head, None, 10_000).await?;
        let revs: Vec<GistRevision> = log
            .into_iter()
            .map(|c| GistRevision {
                changeset: Some(c.id),
                author: Some(c.author),
                created_at: Some(ts(c.time_ms)),
                additions: c.additions,
                deletions: c.deletions,
                files_changed: c.paths.len() as u32,
            })
            .collect();
        page_vec(revs, token, limit)
    }

    /// A signed URL for one file's raw content at a revision (default head).
    pub async fn raw_url(
        &self,
        cx: &Cx,
        id: &str,
        filename: &str,
        rev: Option<&ObjectId>,
    ) -> Result<(String, u64, u64)> {
        let g = self.get(cx, id, rev, 0).await?;
        let f = g
            .files
            .iter()
            .find(|f| f.filename == filename)
            .ok_or_else(|| ForgeError::not_found(format!("gist file {filename}")))?;
        let blob = f.blob.clone().unwrap_or_default();
        let signed = self
            .signer
            .download(&format!("gists/{id}/{}", hex(&blob.raw)), self.db.now_ms());
        Ok((signed.url, signed.expires_ms, f.size))
    }

    /// Forks a gist for the caller (same visibility, copied history).
    pub async fn fork(&self, cx: &Cx, id: &str) -> Result<Gist> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut prec, mut prow) = Self::load_locked(&mut tx, id).await?;
        let mut parent = prec.gist.clone().unwrap_or_default();
        if parent.owner.as_ref().map(|o| o.id.as_str()) == Some(cx.actor_id.as_str()) {
            return Err(
                ForgeError::invalid_state("you cannot fork your own gist").with_subcode("own_gist")
            );
        }
        let now = tx.now_ms();
        let new_id = tx.new_id()?;
        self.vcs
            .fork(&VcsRepo::gist(id), &VcsRepo::gist(&new_id))
            .await?;
        let g = Gist {
            id: new_id.clone(),
            owner: Some(cx.principal()),
            description: parent.description.clone(),
            public: parent.public,
            fork_of: id.to_string(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        let scope = if g.public { "pub" } else { "sec" };
        let mut row = Row::new(scope, new_id)
            .k1(cx.actor_id.clone())
            .k2(id.to_string())
            .n1(now as i64);
        row.body = GistRec {
            gist: Some(g.clone()),
        }
        .encode_to_vec();
        tx.insert(&GISTS, &row).await?;
        parent.fork_count += 1;
        prec.gist = Some(parent);
        prow.body = prec.encode_to_vec();
        tx.update(&GISTS, &prow).await?;
        Self::emit_gist(
            &mut tx,
            "gist.forked",
            "gist.fork",
            &g,
            serde_json::json!({"fork_of": id}),
        )
        .await?;
        tx.commit().await?;
        self.view(g, None, DEFAULT_INLINE).await
    }

    /// Forks of a gist, newest first.
    pub async fn forks(
        &self,
        cx: &Cx,
        id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Gist>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load(&mut tx, id).await?;
        let mut recs: Vec<(Gist, i64)> = Vec::new();
        for scope in ["pub", "sec"] {
            for r in tx
                .scan(
                    &GISTS,
                    &Scan::scope(scope, 5000).key(Key::K2(id.to_string())),
                )
                .await?
            {
                recs.push((
                    r.decode::<GistRec>()?.gist.unwrap_or_default(),
                    r.n1.unwrap_or(0),
                ));
            }
        }
        recs.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.id.cmp(&a.0.id)));
        let (page, next) = page_vec(recs, token, limit)?;
        drop(tx);
        Ok((self.views(page).await?, next))
    }

    /// Stars or unstars a gist for the caller (idempotent).
    pub async fn set_starred(&self, cx: &Cx, id: &str, starred: bool) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, id).await?;
        let mut g = rec.gist.clone().unwrap_or_default();
        let has = tx.get(&STARS, id, &cx.actor_id).await?.is_some();
        if has == starred {
            return Ok(());
        }
        if starred {
            tx.insert(
                &STARS,
                &Row::new(id, cx.actor_id.clone())
                    .k1(cx.actor_id.clone())
                    .n1(tx.now_ms() as i64),
            )
            .await?;
            g.star_count += 1;
        } else {
            tx.delete(&STARS, id, &cx.actor_id).await?;
            g.star_count = g.star_count.saturating_sub(1);
        }
        rec.gist = Some(g.clone());
        row.body = rec.encode_to_vec();
        tx.update(&GISTS, &row).await?;
        let (kind, audit) = if starred {
            ("gist.starred", "gist.star")
        } else {
            ("gist.unstarred", "gist.unstar")
        };
        Self::emit_gist(
            &mut tx,
            kind,
            audit,
            &g,
            serde_json::json!({"star_count": g.star_count}),
        )
        .await?;
        tx.commit().await
    }

    /// Whether the caller starred a gist.
    pub async fn is_starred(&self, cx: &Cx, id: &str) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load(&mut tx, id).await?;
        Ok(tx.get(&STARS, id, &cx.actor_id).await?.is_some())
    }

    /// Adds a comment (owners may comment on a locked gist).
    pub async fn create_comment(&self, cx: &Cx, id: &str, body: &str) -> Result<Comment> {
        required("body", body)?;
        max_len("body", body, MAX_BODY)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, id).await?;
        let mut g = rec.gist.clone().unwrap_or_default();
        let owner = g.owner.as_ref().map(|o| o.id.as_str()) == Some(cx.actor_id.as_str());
        if g.comments_locked && !owner {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Locked,
                "comments on this gist are locked",
            ));
        }
        let now = tx.now_ms();
        let cid = tx.new_id()?;
        let c = Comment {
            id: cid.clone(),
            subject: Some(subject(SubjectKind::Gist, "", id)),
            author: Some(cx.principal()),
            body: body.to_string(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            author_association: if owner { "OWNER".into() } else { "NONE".into() },
            url: format!("{}/gists/{id}#gistcomment-{cid}", self.base_url),
            ..Default::default()
        };
        tx.insert(&COMMENTS, &Row::new(id, cid).n1(now as i64).msg(&c))
            .await?;
        g.comment_count += 1;
        rec.gist = Some(g.clone());
        row.body = rec.encode_to_vec();
        tx.update(&GISTS, &row).await?;
        let payload = serde_json::json!({"gist_id": id, "owner": g.owner, "comment": &c});
        emit(
            &mut tx,
            "gist.commented",
            "",
            "",
            subject(SubjectKind::Gist, "", id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "gist.comment",
            "",
            "",
            SubjectKind::Gist,
            id,
            &serde_json::json!({"comment_id": c.id}),
        ))
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Comments, oldest first.
    pub async fn list_comments(
        &self,
        cx: &Cx,
        id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Comment>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load(&mut tx, id).await?;
        let (rows, next) = paged(
            &mut tx,
            &COMMENTS,
            Scan::scope(id, limit).by_n1(),
            token,
            limit,
        )
        .await?;
        Ok((
            rows.iter()
                .map(|r| r.decode::<Comment>())
                .collect::<Result<Vec<_>>>()?,
            next,
        ))
    }

    /// The owner of a gist.
    pub async fn owner(&self, cx: &Cx, id: &str) -> Result<Principal> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, id).await?;
        Ok(rec.gist.and_then(|g| g.owner).unwrap_or_default())
    }

    /// Number of public gists owned by `user_id`.
    pub async fn public_count(&self, cx: &Cx, user_id: &str) -> Result<u64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.count(&GISTS, "pub", Some(&Key::K1(user_id.to_string())))
            .await
    }
}
