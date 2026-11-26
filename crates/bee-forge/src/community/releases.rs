//! Releases (PRODUCT §4.16, F416-021/022, AS-COLLAB-01): tags, notes, drafts, prereleases,
//! latest, immutable releases, assets, generated notes and source archive requests.
//!
//! # Lifecycle
//!
//! A release is a tag plus notes. A **draft** has no tag yet (the target is resolved and kept;
//! the tag is created at publish). Publishing creates the tag if missing, stamps
//! `published_at`, emits `release.published` and, when a discussion category was named,
//! opens the linked discussion. **Prereleases** never become latest. **Latest** is the
//! published, non-prerelease release with the newest `latest_at` among those not pinned off
//! with `make_latest = FALSE`; `latest_at` is the publish time, or the moment of an explicit
//! `make_latest = TRUE`.
//!
//! An **immutable** release (chosen while it is a draft) freezes on publish: its tag, target
//! and assets cannot change, its tag cannot be deleted or reused after the release is
//! deleted (a tombstone keeps the name), and a signing worker can attach an attestation
//! ([`Releases::record_attestation`]). Titles and notes stay editable.
//!
//! # Generated notes
//!
//! [`Releases::generate_notes`] lists the change requests landed in the history of the new
//! tag but not of the previous one, through the [`ChangeSource`](super::ports::ChangeSource)
//! port. The previous tag is the one named, else the newest published release's tag. Layout:
//! `## What's Changed` with `* <title> by @<author> in #<number>` per change request in
//! landing order, `## New Contributors` for authors with no earlier landed change, and
//! `**Full Changelog**: <compare link>`. A `.github/release.yml` (or `config_path`) at the new
//! tag may exclude labels and authors and group changes into titled categories by label
//! (`*` matches any). The output is a pure function of the port's data and the config.
//!
//! # Assets
//!
//! Upload is two steps: `begin` creates an `UPLOADING` asset and returns a signed PUT URL;
//! `finalize` records the SHA-256 (which is also the blob-store content id) and moves it to
//! `UPLOADED`. Download URLs are signed and count downloads. Source archive requests are
//! recorded per `(tree, format, prefix)` for the archive worker
//! ([`Releases::pending_archives`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::{
    ObjectId, Principal, Release, ReleaseAsset, ReleaseAssetState, ReleaseAttestation,
    ReleaseMakeLatest, Repo, RepoTag, RepoTagAnnotation, Signature, SourceArchiveFormat,
    SubjectKind, SubjectRef,
};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::hooks::RevisionResolver;
use crate::ids::{hex, ts};
use crate::module::EventKind;
use crate::store::{Db, Index, Key, Row, Scan, TableDef, Tx};

use super::common::{MAX_BODY, MAX_TITLE, draft, emit, max_len, page_vec, required, subject};
use super::discussions::{Discussions, Home, NewDiscussion};
use super::ports::{
    ChangeSource, ContentVcs, LandedChange, NewAnnotation, SignedUrl, TagInfo, TagVcs, UrlSigner,
    VcsRepo, read_file,
};
use super::{SURFACE, yaml};

/// Releases, scoped by repo; `k1` tag name (unique), `n1` created.
pub static RELEASES: TableDef = TableDef {
    name: "forge_releases",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};
/// Release assets, scoped by repo; `k1` release id, `k2` `<release id>:<name>` (unique).
pub static ASSETS: TableDef = TableDef {
    name: "forge_release_assets",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: true,
    indexes: &[Index::ScopeK1, Index::ScopeK2],
};
/// Tag names of deleted immutable releases, which can never be reused.
pub static TOMBSTONES: TableDef = TableDef {
    name: "forge_release_tombstones",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Source archive requests, scoped by repo; `k1` request key (unique), `n1` requested.
pub static ARCHIVES: TableDef = TableDef {
    name: "forge_release_archives",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1, Index::N1],
};
/// Release attestations, scoped by repo, keyed by release id.
pub static ATTESTATIONS: TableDef = TableDef {
    name: "forge_release_attestations",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Release event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::gh(
        "release.created",
        "A release (or draft) was created",
        "release",
        "created",
    ),
    EventKind::gh(
        "release.published",
        "A release was published",
        "release",
        "published",
    ),
    EventKind::gh(
        "release.edited",
        "A release was edited",
        "release",
        "edited",
    ),
    EventKind::gh(
        "release.deleted",
        "A release was deleted",
        "release",
        "deleted",
    ),
    EventKind::new("release.asset_uploaded", "A release asset was uploaded"),
    EventKind::new("release.asset_deleted", "A release asset was deleted"),
    EventKind::new(
        "release.discussion_linked",
        "A discussion was linked to a release",
    ),
    EventKind::new(
        "release.attested",
        "An attestation was recorded for a release",
    ),
    EventKind::new(
        "release.archive_requested",
        "A source archive was requested",
    ),
    EventKind::new("tag.created", "A tag was created"),
    EventKind::new("tag.deleted", "A tag was deleted"),
];

/// Most assets on one release.
pub const MAX_ASSETS: usize = 1000;
/// Largest asset.
pub const MAX_ASSET_BYTES: u64 = 2 << 30;
/// Most releases the store lists or ranks per repo.
pub const MAX_RELEASES: usize = 10_000;

/// A release's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct ReleaseRec {
    /// The release without assets and without computed fields.
    #[prost(message, optional, tag = "1")]
    pub release: Option<Release>,
    /// [`ReleaseMakeLatest`].
    #[prost(int32, tag = "2")]
    pub make_latest: i32,
    /// When it became (or was pinned as) latest candidate, ms.
    #[prost(uint64, tag = "3")]
    pub latest_at: u64,
    /// Discussion category to open at publish (drafts) or that was used.
    #[prost(string, tag = "4")]
    pub category: String,
}

/// An asset's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct AssetRec {
    /// The asset.
    #[prost(message, optional, tag = "1")]
    pub asset: Option<ReleaseAsset>,
    /// Blob-store key of the bytes.
    #[prost(string, tag = "2")]
    pub key: String,
}

/// A source archive request.
#[derive(Clone, PartialEq, Message)]
pub struct ArchiveRec {
    /// Request id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Repo id.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// [`SourceArchiveFormat`].
    #[prost(int32, tag = "3")]
    pub format: i32,
    /// Top-level directory name.
    #[prost(string, tag = "4")]
    pub prefix: String,
    /// Changeset.
    #[prost(bytes = "vec", tag = "5")]
    pub changeset: Vec<u8>,
    /// Tree id.
    #[prost(bytes = "vec", tag = "6")]
    pub tree: Vec<u8>,
    /// Blob-store key of the archive.
    #[prost(string, tag = "7")]
    pub key: String,
    /// Who asked first.
    #[prost(string, tag = "8")]
    pub requested_by: String,
    /// Requests for the same archive.
    #[prost(uint32, tag = "9")]
    pub requests: u32,
    /// The worker built it.
    #[prost(bool, tag = "10")]
    pub built: bool,
}

/// What `CreateRelease` needs.
#[derive(Clone, Debug, Default)]
pub struct NewRelease {
    /// Tag name.
    pub tag_name: String,
    /// Revision for a tag that does not exist yet (empty = default ref).
    pub target: String,
    /// Title.
    pub name: String,
    /// Notes.
    pub body: String,
    /// Create as a draft.
    pub draft: bool,
    /// Prerelease.
    pub prerelease: bool,
    /// Latest policy.
    pub make_latest: i32,
    /// Freeze on publish.
    pub immutable: bool,
    /// Prepend generated notes.
    pub generate_notes: bool,
    /// Discussion category to open.
    pub discussion_category: String,
    /// Base for generated notes.
    pub previous_tag: String,
}

/// A partial release update.
#[derive(Clone, Debug, Default)]
pub struct ReleasePatch {
    /// New tag (drafts only).
    pub tag_name: Option<String>,
    /// New target revision (drafts only).
    pub target: Option<String>,
    /// Title.
    pub name: Option<String>,
    /// Notes.
    pub body: Option<String>,
    /// `Some(false)` publishes.
    pub draft: Option<bool>,
    /// Prerelease.
    pub prerelease: Option<bool>,
    /// Latest policy.
    pub make_latest: Option<i32>,
    /// Immutability (drafts only).
    pub immutable: Option<bool>,
    /// Discussion category.
    pub discussion_category: Option<String>,
    /// Expected version, 0 = any.
    pub expected_version: u64,
}

/// Generated release notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notes {
    /// Suggested title (the tag name).
    pub name: String,
    /// Markdown body.
    pub body: String,
    /// Change requests included, in order.
    pub change_requests: Vec<SubjectRef>,
    /// The previous tag actually used (empty for the first release).
    pub previous_tag_name: String,
}

/// An asset upload ticket.
#[derive(Clone, Debug)]
pub struct UploadTicket {
    /// Asset id.
    pub asset_id: String,
    /// Signed PUT URL and headers.
    pub url: SignedUrl,
}

/// Checks a tag name against git's ref-name rules (the subset that matters here).
pub fn valid_tag(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.len() > 255
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name.starts_with('-')
        || name.split('/').any(|c| c.starts_with('.'))
        || name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c));
    if bad {
        Err(ForgeError::invalid(
            "invalid_tag",
            format!("{name:?} is not a valid tag name"),
        ))
    } else {
        Ok(())
    }
}

/// Parsed `release.yml` changelog settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotesConfig {
    /// Labels whose change requests are left out.
    pub exclude_labels: Vec<String>,
    /// Authors whose change requests are left out.
    pub exclude_authors: Vec<String>,
    /// Categories in order.
    pub categories: Vec<NotesCategory>,
}

/// One notes category.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotesCategory {
    /// Section title.
    pub title: String,
    /// Labels that select it (`*` = any).
    pub labels: Vec<String>,
    /// Labels that keep a change request out of this category.
    pub exclude_labels: Vec<String>,
    /// Authors kept out of this category.
    pub exclude_authors: Vec<String>,
}

/// Parses a notes config; an empty file is the default config.
pub fn parse_notes_config(src: &str) -> Result<NotesConfig> {
    let y = yaml::parse(src).map_err(|e| ForgeError::invalid("invalid_release_config", e))?;
    let mut cfg = NotesConfig::default();
    let Some(cl) = y.get("changelog") else {
        return Ok(cfg);
    };
    if let Some(ex) = cl.get("exclude") {
        cfg.exclude_labels = ex.get("labels").map(|l| l.strings()).unwrap_or_default();
        cfg.exclude_authors = ex.get("authors").map(|l| l.strings()).unwrap_or_default();
    }
    for c in cl.get("categories").map(|c| c.items()).unwrap_or_default() {
        cfg.categories.push(NotesCategory {
            title: c.get("title").and_then(|t| t.as_str()).unwrap_or_default(),
            labels: c.get("labels").map(|l| l.strings()).unwrap_or_default(),
            exclude_labels: c
                .get("exclude")
                .and_then(|e| e.get("labels"))
                .map(|l| l.strings())
                .unwrap_or_default(),
            exclude_authors: c
                .get("exclude")
                .and_then(|e| e.get("authors"))
                .map(|l| l.strings())
                .unwrap_or_default(),
        });
    }
    Ok(cfg)
}

fn login_of(p: &Principal) -> String {
    if p.display.is_empty() {
        p.id.clone()
    } else {
        p.display.clone()
    }
}

/// Builds the notes body from landed changes. Pure: the same inputs give the same text.
pub fn render_notes(
    cfg: &NotesConfig,
    changes: &[LandedChange],
    earlier_authors: Option<&BTreeSet<String>>,
    compare: Option<(&str, &str, &str)>,
) -> (String, Vec<SubjectRef>, Vec<u64>) {
    let line = |c: &LandedChange| {
        format!(
            "* {} by @{} in #{}",
            c.title.trim(),
            login_of(&c.author),
            c.number
        )
    };
    let excluded = |c: &LandedChange| {
        c.labels
            .iter()
            .any(|l| cfg.exclude_labels.iter().any(|e| e.eq_ignore_ascii_case(l)))
            || cfg
                .exclude_authors
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&login_of(&c.author)) || *a == c.author.id)
    };
    let kept: Vec<&LandedChange> = changes.iter().filter(|c| !excluded(c)).collect();
    let mut out = String::new();
    if cfg.categories.is_empty() {
        if !kept.is_empty() {
            out.push_str("## What's Changed\n");
            for c in &kept {
                out.push_str(&line(c));
                out.push('\n');
            }
        }
    } else {
        let mut sections: Vec<(String, Vec<&LandedChange>)> = cfg
            .categories
            .iter()
            .map(|c| (c.title.clone(), Vec::new()))
            .collect();
        let mut other: Vec<&LandedChange> = Vec::new();
        for c in &kept {
            let slot = cfg.categories.iter().position(|cat| {
                let label_ok = cat
                    .labels
                    .iter()
                    .any(|l| l == "*" || c.labels.iter().any(|x| x.eq_ignore_ascii_case(l)));
                let label_out = c
                    .labels
                    .iter()
                    .any(|x| cat.exclude_labels.iter().any(|e| e.eq_ignore_ascii_case(x)));
                let author_out = cat
                    .exclude_authors
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&login_of(&c.author)) || *a == c.author.id);
                label_ok && !label_out && !author_out
            });
            match slot {
                Some(i) => sections[i].1.push(c),
                None => other.push(c),
            }
        }
        for (title, items) in sections
            .iter()
            .chain(std::iter::once(&("Other Changes".to_string(), other)))
        {
            if items.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("## {title}\n"));
            for c in items {
                out.push_str(&line(c));
                out.push('\n');
            }
        }
    }
    if let Some(earlier) = earlier_authors {
        let mut seen = BTreeSet::new();
        let mut firsts = Vec::new();
        for c in &kept {
            let id = if c.author.id.is_empty() {
                login_of(&c.author)
            } else {
                c.author.id.clone()
            };
            if !earlier.contains(&id) && seen.insert(id) {
                firsts.push(*c);
            }
        }
        if !firsts.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("## New Contributors\n");
            for c in firsts {
                out.push_str(&format!(
                    "* @{} made their first contribution in #{}\n",
                    login_of(&c.author),
                    c.number
                ));
            }
        }
    }
    if let Some((base, prev, tag)) = compare {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "**Full Changelog**: {base}/compare/{prev}...{tag}\n"
        ));
    }
    let refs = kept
        .iter()
        .map(|c| SubjectRef {
            kind: SubjectKind::ChangeRequest as i32,
            repo: None,
            id: c.id.clone(),
        })
        .collect();
    let numbers = kept.iter().map(|c| c.number).collect();
    (out, refs, numbers)
}

/// Releases over the store and ports.
pub struct Releases {
    db: Db,
    tags: Arc<dyn TagVcs>,
    changes: Arc<dyn ChangeSource>,
    vcs: Arc<dyn ContentVcs>,
    resolver: Arc<dyn RevisionResolver>,
    signer: Arc<dyn UrlSigner>,
    discussions: Arc<Discussions>,
    base_url: String,
}

impl Releases {
    /// Releases over the given store and ports.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        tags: Arc<dyn TagVcs>,
        changes: Arc<dyn ChangeSource>,
        vcs: Arc<dyn ContentVcs>,
        resolver: Arc<dyn RevisionResolver>,
        signer: Arc<dyn UrlSigner>,
        discussions: Arc<Discussions>,
        base_url: &str,
    ) -> Releases {
        Releases {
            db,
            tags,
            changes,
            vcs,
            resolver,
            signer,
            discussions,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn resolve(&self, repo: &Repo, expr: &str) -> Result<ObjectId> {
        let expr = if expr.is_empty() {
            repo.default_ref.as_str()
        } else {
            expr
        };
        self.resolver.resolve(repo, expr).await
    }

    fn urls(&self, repo: &Repo, r: &mut Release) {
        r.url = format!(
            "{}/{}/releases/tag/{}",
            self.base_url, repo.full_name, r.tag_name
        );
        r.zipball_url = format!(
            "{}/{}/archive/{}.zip",
            self.base_url, repo.full_name, r.tag_name
        );
        r.tarball_url = format!(
            "{}/{}/archive/{}.tar.gz",
            self.base_url, repo.full_name, r.tag_name
        );
        r.repo = super::common::repo_ref_id(&repo.id);
    }

    async fn all(tx: &mut Tx, repo_id: &str) -> Result<Vec<(ReleaseRec, Row)>> {
        tx.scan_msgs::<ReleaseRec>(
            &RELEASES,
            &Scan::scope(repo_id, MAX_RELEASES).by_n1().desc(),
        )
        .await
    }

    fn latest_id(recs: &[(ReleaseRec, Row)]) -> Option<String> {
        recs.iter()
            .filter(|(r, _)| {
                r.release
                    .as_ref()
                    .is_some_and(|x| !x.draft && !x.prerelease)
                    && r.make_latest != ReleaseMakeLatest::False as i32
            })
            .max_by_key(|(r, row)| (r.latest_at, row.id.clone()))
            .map(|(_, row)| row.id.clone())
    }

    async fn assets_of(tx: &mut Tx, repo_id: &str, release_id: &str) -> Result<Vec<ReleaseAsset>> {
        let rows = tx
            .scan_msgs::<AssetRec>(
                &ASSETS,
                &Scan::scope(repo_id, MAX_ASSETS).key(Key::K1(release_id.to_string())),
            )
            .await?;
        let mut v: Vec<ReleaseAsset> = rows.into_iter().filter_map(|(a, _)| a.asset).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn view(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        rec: &ReleaseRec,
        row: &Row,
        latest: &Option<String>,
    ) -> Result<Release> {
        let mut r = rec.release.clone().unwrap_or_default();
        r.id = row.id.clone();
        r.latest = latest.as_deref() == Some(row.id.as_str());
        r.version = row.version;
        r.assets = Self::assets_of(tx, &repo.id, &row.id).await?;
        self.urls(repo, &mut r);
        Ok(r)
    }

    async fn load(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(ReleaseRec, Row)> {
        tx.get_msg::<ReleaseRec>(&RELEASES, repo_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("release {id}")))
    }

    async fn load_locked(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(ReleaseRec, Row)> {
        match tx.get_for_update(&RELEASES, repo_id, id).await? {
            Some(row) => Ok((row.decode()?, row)),
            None => Err(ForgeError::not_found(format!("release {id}"))),
        }
    }

    fn hide_draft(rec: &ReleaseRec, see_drafts: bool) -> Result<()> {
        if rec.release.as_ref().is_some_and(|r| r.draft) && !see_drafts {
            Err(ForgeError::not_found("release"))
        } else {
            Ok(())
        }
    }

    /// Creates a release.
    pub async fn create(&self, cx: &Cx, repo: &Repo, n: NewRelease) -> Result<Release> {
        valid_tag(&n.tag_name)?;
        max_len("name", &n.name, MAX_TITLE)?;
        max_len("body", &n.body, MAX_BODY)?;
        if n.immutable && !n.draft {
            // GitHub's rule: immutability is chosen while drafting, so assets can be attached first.
            return Err(ForgeError::invalid(
                "immutable_needs_draft",
                "an immutable release is created as a draft, then published",
            ));
        }
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if !tx
                .scan(
                    &RELEASES,
                    &Scan::scope(&repo.id, 1).key(Key::K1(n.tag_name.clone())),
                )
                .await?
                .is_empty()
            {
                return Err(ForgeError::already_exists(format!(
                    "a release for tag {}",
                    n.tag_name
                )));
            }
            if tx.get(&TOMBSTONES, &repo.id, &n.tag_name).await?.is_some() {
                return Err(ForgeError::already_exists(format!("tag {}", n.tag_name))
                    .with_subcode("tag_immutable"));
            }
        }
        let existing = self.tags.get(&repo.id, &n.tag_name).await?;
        let (target, target_ref) = match &existing {
            Some(t) => (t.target.clone(), String::new()),
            None => {
                let expr = if n.target.is_empty() {
                    repo.default_ref.clone()
                } else {
                    n.target.clone()
                };
                (self.resolve(repo, &expr).await?, expr)
            }
        };
        let mut body = n.body.clone();
        if n.generate_notes {
            let notes = self
                .notes_for(repo, &n.tag_name, &target, &n.previous_tag, "")
                .await?;
            body = if body.trim().is_empty() {
                notes.body
            } else {
                format!("{}\n{}", notes.body.trim_end(), body)
            };
            max_len("body", &body, MAX_BODY)?;
        }
        let home = Home::of_repo(repo);
        let mut discussion_id = String::new();
        if !n.draft && !n.discussion_category.is_empty() {
            let title = if n.name.is_empty() {
                n.tag_name.clone()
            } else {
                n.name.clone()
            };
            discussion_id = self
                .open_discussion(
                    cx,
                    &home,
                    &n.discussion_category,
                    &title,
                    &body,
                    &n.tag_name,
                )
                .await?;
        }
        if !n.draft && existing.is_none() {
            self.tags
                .create(&repo.id, &n.tag_name, &target, None)
                .await?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let make_latest = if n.make_latest == 0 {
            ReleaseMakeLatest::Legacy as i32
        } else {
            n.make_latest
        };
        let rel = Release {
            id: id.clone(),
            tag_name: n.tag_name.clone(),
            target: Some(target),
            target_ref,
            name: n.name.clone(),
            body,
            draft: n.draft,
            prerelease: n.prerelease,
            immutable: n.immutable,
            author: Some(cx.principal()),
            created_at: Some(ts(now)),
            published_at: (!n.draft).then(|| ts(now)),
            updated_at: Some(ts(now)),
            discussion_id: discussion_id.clone(),
            discussion_category_id: n.discussion_category.clone(),
            ..Default::default()
        };
        let rec = ReleaseRec {
            release: Some(rel),
            make_latest,
            latest_at: if n.draft { 0 } else { now },
            category: n.discussion_category,
        };
        let row = Row::new(&repo.id, id.clone())
            .k1(n.tag_name.clone())
            .n1(now as i64)
            .msg(&rec);
        tx.insert(&RELEASES, &row).await?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let latest = Self::latest_id(&all);
        let out = self
            .view(&mut tx, repo, &rec, &row.version(1), &latest)
            .await?;
        self.record(&mut tx, repo, "release.created", "release.create", &out)
            .await?;
        if !out.draft {
            self.record(&mut tx, repo, "release.published", "release.publish", &out)
                .await?;
        }
        tx.commit().await?;
        Ok(out)
    }

    async fn open_discussion(
        &self,
        cx: &Cx,
        home: &Home,
        category: &str,
        title: &str,
        body: &str,
        tag: &str,
    ) -> Result<String> {
        let body = if body.trim().is_empty() {
            format!("Discussion for release {tag}.")
        } else {
            body.to_string()
        };
        let d = self
            .discussions
            .create(
                cx,
                home,
                NewDiscussion {
                    category: category.to_string(),
                    title: title.to_string(),
                    body,
                    moderator: true,
                    ..Default::default()
                },
            )
            .await?;
        Ok(d.id)
    }

    async fn record(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        kind: &str,
        audit: &str,
        r: &Release,
    ) -> Result<()> {
        let action = kind.split_once('.').map(|(_, a)| a).unwrap_or(kind);
        let payload = serde_json::json!({"action": action, "release": r});
        emit(
            tx,
            kind,
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Release, &repo.id, &r.id),
            payload,
        )
        .await?;
        tx.audit(draft(audit, &repo.org_id, &repo.id, SubjectKind::Release, &r.id,
            &serde_json::json!({"tag": r.tag_name, "draft": r.draft, "prerelease": r.prerelease, "immutable": r.immutable}))).await
    }

    /// One release by id. Drafts are visible only with `see_drafts`.
    pub async fn get(&self, cx: &Cx, repo: &Repo, id: &str, see_drafts: bool) -> Result<Release> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        Self::hide_draft(&rec, see_drafts)?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let latest = Self::latest_id(&all);
        self.view(&mut tx, repo, &rec, &row, &latest).await
    }

    /// The release of a tag.
    pub async fn get_by_tag(
        &self,
        cx: &Cx,
        repo: &Repo,
        tag: &str,
        see_drafts: bool,
    ) -> Result<Release> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &RELEASES,
                &Scan::scope(&repo.id, 1).key(Key::K1(tag.to_string())),
            )
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Err(ForgeError::not_found(format!("release for tag {tag}")));
        };
        let rec: ReleaseRec = row.decode()?;
        Self::hide_draft(&rec, see_drafts)?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let latest = Self::latest_id(&all);
        self.view(&mut tx, repo, &rec, &row, &latest).await
    }

    /// The latest release.
    pub async fn latest(&self, cx: &Cx, repo: &Repo) -> Result<Release> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let Some(id) = Self::latest_id(&all) else {
            return Err(ForgeError::not_found("a published release"));
        };
        let (rec, row) = all
            .into_iter()
            .find(|(_, r)| r.id == id)
            .ok_or_else(|| ForgeError::not_found("release"))?;
        self.view(&mut tx, repo, &rec, &row, &Some(id)).await
    }

    /// Releases, newest first. Drafts only with `see_drafts`; prereleases only when asked.
    pub async fn list(
        &self,
        cx: &Cx,
        repo: &Repo,
        see_drafts: bool,
        prereleases: bool,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Release>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let latest = Self::latest_id(&all);
        let visible: Vec<(ReleaseRec, Row)> = all
            .into_iter()
            .filter(|(r, _)| {
                let x = r.release.as_ref();
                (see_drafts || !x.is_some_and(|x| x.draft))
                    && (prereleases || !x.is_some_and(|x| x.prerelease))
            })
            .collect();
        let (page, next) = page_vec(visible, token, limit)?;
        let mut out = Vec::new();
        for (rec, row) in page {
            out.push(self.view(&mut tx, repo, &rec, &row, &latest).await?);
        }
        Ok((out, next))
    }

    /// Updates a release; `draft = false` publishes.
    pub async fn update(&self, cx: &Cx, repo: &Repo, id: &str, p: ReleasePatch) -> Result<Release> {
        let (rec0, _) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::load(&mut tx, &repo.id, id).await?
        };
        let cur = rec0.release.clone().unwrap_or_default();
        if let Some(n) = &p.name {
            max_len("name", n, MAX_TITLE)?;
        }
        if let Some(b) = &p.body {
            max_len("body", b, MAX_BODY)?;
        }
        if let Some(t) = &p.tag_name {
            valid_tag(t)?;
        }
        let publishing = cur.draft && p.draft == Some(false);
        let new_target = match (&p.target, &p.tag_name) {
            (Some(t), _) => Some(self.resolve(repo, t).await?),
            _ => None,
        };
        let tag = p.tag_name.clone().unwrap_or_else(|| cur.tag_name.clone());
        let mut tag_info: Option<TagInfo> = None;
        if publishing {
            tag_info = self.tags.get(&repo.id, &tag).await?;
            if tag_info.is_none() {
                let target = new_target
                    .clone()
                    .or(cur.target.clone())
                    .ok_or_else(|| ForgeError::internal("release has no target"))?;
                tag_info = Some(self.tags.create(&repo.id, &tag, &target, None).await?);
            }
        }
        let mut discussion_id = String::new();
        let category = p
            .discussion_category
            .clone()
            .unwrap_or_else(|| rec0.category.clone());
        let want_discussion = (publishing || (!cur.draft && p.discussion_category.is_some()))
            && !category.is_empty()
            && cur.discussion_id.is_empty();
        if want_discussion {
            let name = p.name.clone().unwrap_or(cur.name.clone());
            let title = if name.is_empty() { tag.clone() } else { name };
            let body = p.body.clone().unwrap_or(cur.body.clone());
            discussion_id = self
                .open_discussion(cx, &Home::of_repo(repo), &category, &title, &body, &tag)
                .await?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, &repo.id, id).await?;
        let cur = rec.release.clone().unwrap_or_default();
        let frozen = !cur.draft && cur.immutable;
        if frozen && (p.tag_name.is_some() || p.target.is_some() || p.immutable.is_some()) {
            return Err(ForgeError::invalid_state("the release is immutable")
                .with_subcode("immutable_release"));
        }
        if !cur.draft && (p.tag_name.is_some() || p.target.is_some() || p.immutable.is_some()) {
            return Err(ForgeError::invalid_state(
                "the tag, target and immutability are fixed once published",
            )
            .with_subcode("tag_locked"));
        }
        let publishing = cur.draft && p.draft == Some(false);
        if !cur.draft && p.draft == Some(true) && cur.immutable {
            return Err(
                ForgeError::invalid_state("an immutable release cannot go back to draft")
                    .with_subcode("immutable_release"),
            );
        }
        if p.expected_version != 0 && p.expected_version != row.version {
            return Err(ForgeError::version_mismatch(
                p.expected_version,
                row.version,
            ));
        }
        let now = tx.now_ms();
        let mut r = rec.release.clone().unwrap_or_default();
        if let Some(t) = &p.tag_name
            && *t != r.tag_name
        {
            if !tx
                .scan(&RELEASES, &Scan::scope(&repo.id, 1).key(Key::K1(t.clone())))
                .await?
                .is_empty()
            {
                return Err(ForgeError::already_exists(format!("a release for tag {t}")));
            }
            r.tag_name = t.clone();
            row.k1 = Some(t.clone());
        }
        if let Some(t) = new_target {
            r.target = Some(t);
            r.target_ref = p.target.clone().unwrap_or_default();
        }
        if let Some(n) = p.name {
            r.name = n;
        }
        if let Some(b) = p.body {
            r.body = b;
        }
        if let Some(x) = p.prerelease {
            r.prerelease = x;
        }
        if let Some(x) = p.immutable {
            r.immutable = x;
        }
        if let Some(ml) = p.make_latest {
            rec.make_latest = if ml == 0 {
                ReleaseMakeLatest::Legacy as i32
            } else {
                ml
            };
            if rec.make_latest == ReleaseMakeLatest::True as i32 {
                rec.latest_at = now;
            }
        }
        if let Some(c) = &p.discussion_category {
            rec.category = c.clone();
            r.discussion_category_id = c.clone();
        }
        if !discussion_id.is_empty() {
            r.discussion_id = discussion_id;
        }
        if publishing {
            r.draft = false;
            r.published_at = Some(ts(now));
            if let Some(t) = &tag_info {
                r.target = Some(t.target.clone());
            }
            if rec.latest_at < now && rec.make_latest != ReleaseMakeLatest::True as i32 {
                rec.latest_at = now;
            }
        } else if p.draft == Some(true) && !r.draft {
            r.draft = true;
            r.published_at = None;
        }
        r.updated_at = Some(ts(now));
        rec.release = Some(r);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&RELEASES, &row).await?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let latest = Self::latest_id(&all);
        let out = self.view(&mut tx, repo, &rec, &row, &latest).await?;
        if publishing {
            self.record(&mut tx, repo, "release.published", "release.publish", &out)
                .await?;
        } else {
            self.record(&mut tx, repo, "release.edited", "release.update", &out)
                .await?;
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Publishes a draft.
    pub async fn publish(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        make_latest: i32,
    ) -> Result<Release> {
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
            if !rec.release.is_some_and(|r| r.draft) {
                return Err(ForgeError::invalid_state(
                    "the release is already published",
                ));
            }
        }
        self.update(
            cx,
            repo,
            id,
            ReleasePatch {
                draft: Some(false),
                make_latest: (make_latest != 0).then_some(make_latest),
                ..Default::default()
            },
        )
        .await
    }

    /// Deletes a release, and its tag when asked (never the tag of an immutable release).
    pub async fn delete(&self, cx: &Cx, repo: &Repo, id: &str, delete_tag: bool) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load_locked(&mut tx, &repo.id, id).await?;
        let r = rec.release.clone().unwrap_or_default();
        let frozen = !r.draft && r.immutable;
        if delete_tag && frozen {
            return Err(ForgeError::invalid_state(
                "the tag of an immutable release cannot be deleted",
            )
            .with_subcode("immutable_release"));
        }
        for a in tx
            .scan(
                &ASSETS,
                &Scan::scope(&repo.id, MAX_ASSETS).key(Key::K1(id.to_string())),
            )
            .await?
        {
            tx.delete(&ASSETS, &repo.id, &a.id).await?;
        }
        tx.delete(&RELEASES, &repo.id, id).await?;
        tx.delete(&ATTESTATIONS, &repo.id, id).await?;
        if frozen {
            tx.put(
                &TOMBSTONES,
                &Row::new(&repo.id, r.tag_name.clone()).n1(tx.now_ms() as i64),
            )
            .await?;
        }
        let mut out = r.clone();
        out.id = row.id.clone();
        self.urls(repo, &mut out);
        self.record(&mut tx, repo, "release.deleted", "release.delete", &out)
            .await?;
        tx.commit().await?;
        if delete_tag && !r.draft {
            match self.tags.delete(&repo.id, &r.tag_name, 0).await {
                Ok(()) => {
                    self.record_tag(cx, repo, "tag.deleted", "tag.delete", &r.tag_name)
                        .await?
                }
                Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Links a discussion in `category` to a release.
    pub async fn link_discussion(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        category: &str,
    ) -> Result<Release> {
        required("discussion_category_id", category)?;
        let cur = self.get(cx, repo, id, true).await?;
        if !cur.discussion_id.is_empty() {
            return Err(
                ForgeError::already_exists("a linked discussion").with_subcode("discussion_linked")
            );
        }
        if cur.draft {
            return self
                .update(
                    cx,
                    repo,
                    id,
                    ReleasePatch {
                        discussion_category: Some(category.to_string()),
                        ..Default::default()
                    },
                )
                .await;
        }
        let title = if cur.name.is_empty() {
            cur.tag_name.clone()
        } else {
            cur.name.clone()
        };
        let did = self
            .open_discussion(
                cx,
                &Home::of_repo(repo),
                category,
                &title,
                &cur.body,
                &cur.tag_name,
            )
            .await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, &repo.id, id).await?;
        let mut r = rec.release.clone().unwrap_or_default();
        r.discussion_id = did;
        r.discussion_category_id = category.to_string();
        r.updated_at = Some(ts(tx.now_ms()));
        rec.release = Some(r);
        rec.category = category.to_string();
        row.body = rec.encode_to_vec();
        row.version = tx.update(&RELEASES, &row).await?;
        let all = Self::all(&mut tx, &repo.id).await?;
        let latest = Self::latest_id(&all);
        let out = self.view(&mut tx, repo, &rec, &row, &latest).await?;
        self.record(
            &mut tx,
            repo,
            "release.discussion_linked",
            "release.link_discussion",
            &out,
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    // ---- generated notes ---------------------------------------------------------------

    async fn previous_target(
        &self,
        repo: &Repo,
        tag_name: &str,
        previous: &str,
    ) -> Result<Option<(String, ObjectId)>> {
        if !previous.is_empty() {
            return match self.tags.get(&repo.id, previous).await? {
                Some(t) => Ok(Some((previous.to_string(), t.target))),
                None => Err(ForgeError::invalid(
                    "unknown_previous_tag",
                    format!("tag {previous} does not exist"),
                )),
            };
        }
        let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
        let all = Self::all(&mut tx, &repo.id).await?;
        drop(tx);
        let mut published: Vec<(u64, String)> = all
            .iter()
            .filter_map(|(r, _)| {
                let x = r.release.as_ref()?;
                (!x.draft && x.tag_name != tag_name).then(|| {
                    (
                        x.published_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0),
                        x.tag_name.clone(),
                    )
                })
            })
            .collect();
        published.sort_by(|a, b| b.cmp(a));
        for (_, name) in published {
            if let Some(t) = self.tags.get(&repo.id, &name).await? {
                return Ok(Some((name, t.target)));
            }
        }
        Ok(None)
    }

    async fn notes_for(
        &self,
        repo: &Repo,
        tag_name: &str,
        until: &ObjectId,
        previous: &str,
        config_path: &str,
    ) -> Result<Notes> {
        let prev = self.previous_target(repo, tag_name, previous).await?;
        let since = prev.as_ref().map(|(_, t)| t);
        let changes = self.changes.landed_between(repo, since, until).await?;
        let path = if config_path.is_empty() {
            ".github/release.yml"
        } else {
            config_path
        };
        let vr = VcsRepo::of_repo(repo);
        let cfg = match read_file(self.vcs.as_ref(), &vr, until, path).await {
            Ok(Some((_, bytes))) => parse_notes_config(&String::from_utf8_lossy(&bytes))?,
            Ok(None) => NotesConfig::default(),
            Err(e)
                if e.reason == bee_proto::v1::ErrorReason::NotFound
                    || e.reason == bee_proto::v1::ErrorReason::Unavailable =>
            {
                NotesConfig::default()
            }
            Err(e) => return Err(e),
        };
        let earlier: Option<BTreeSet<String>> = match since {
            Some(s) => Some(
                self.changes
                    .landed_between(repo, None, s)
                    .await?
                    .into_iter()
                    .map(|c| {
                        if c.author.id.is_empty() {
                            login_of(&c.author)
                        } else {
                            c.author.id
                        }
                    })
                    .collect(),
            ),
            None => None,
        };
        let base = format!("{}/{}", self.base_url, repo.full_name);
        let compare = prev
            .as_ref()
            .map(|(p, _)| (base.as_str(), p.as_str(), tag_name));
        let (body, mut refs, _) = render_notes(&cfg, &changes, earlier.as_ref(), compare);
        for r in &mut refs {
            r.repo = super::common::repo_ref_id(&repo.id);
        }
        Ok(Notes {
            name: tag_name.to_string(),
            body,
            change_requests: refs,
            previous_tag_name: prev.map(|(p, _)| p).unwrap_or_default(),
        })
    }

    /// Generated notes for a tag that may not exist yet.
    pub async fn generate_notes(
        &self,
        cx: &Cx,
        repo: &Repo,
        tag_name: &str,
        target: &str,
        previous: &str,
        config_path: &str,
    ) -> Result<Notes> {
        let _ = cx;
        valid_tag(tag_name)?;
        let until = match self.tags.get(&repo.id, tag_name).await? {
            Some(t) => t.target,
            None => self.resolve(repo, target).await?,
        };
        self.notes_for(repo, tag_name, &until, previous, config_path)
            .await
    }

    // ---- assets ------------------------------------------------------------------------

    fn mutable_assets(rec: &ReleaseRec) -> Result<()> {
        let r = rec.release.clone().unwrap_or_default();
        if !r.draft && r.immutable {
            Err(
                ForgeError::invalid_state("the release is immutable; its assets are frozen")
                    .with_subcode("immutable_release"),
            )
        } else {
            Ok(())
        }
    }

    /// Starts an asset upload.
    #[allow(clippy::too_many_arguments)]
    pub async fn begin_asset(
        &self,
        cx: &Cx,
        repo: &Repo,
        release_id: &str,
        name: &str,
        label: &str,
        content_type: &str,
        size: u64,
    ) -> Result<UploadTicket> {
        required("name", name)?;
        max_len("name", name, 255)?;
        max_len("label", label, 255)?;
        if name.contains('/') || name.chars().any(|c| c.is_control()) {
            return Err(ForgeError::invalid(
                "invalid_asset_name",
                "asset names cannot contain `/` or control characters",
            ));
        }
        if size == 0 {
            return Err(ForgeError::invalid(
                "empty_asset",
                "assets must not be empty",
            ));
        }
        if size > MAX_ASSET_BYTES {
            return Err(ForgeError::too_large("asset_bytes", MAX_ASSET_BYTES, size));
        }
        let content_type = if content_type.is_empty() {
            "application/octet-stream"
        } else {
            content_type
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load_locked(&mut tx, &repo.id, release_id).await?;
        Self::mutable_assets(&rec)?;
        let existing = tx
            .scan(
                &ASSETS,
                &Scan::scope(&repo.id, MAX_ASSETS + 1).key(Key::K1(release_id.to_string())),
            )
            .await?;
        if existing.len() >= MAX_ASSETS {
            return Err(ForgeError::too_large(
                "release_assets",
                MAX_ASSETS as u64,
                existing.len() as u64 + 1,
            ));
        }
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let key = format!("releases/{}/{release_id}/{id}", repo.id);
        let asset = ReleaseAsset {
            id: id.clone(),
            release_id: release_id.to_string(),
            name: name.to_string(),
            label: label.to_string(),
            content_type: content_type.to_string(),
            size,
            state: ReleaseAssetState::Uploading as i32,
            uploader: Some(cx.principal()),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        let row = Row::new(&repo.id, id.clone())
            .k1(release_id.to_string())
            .k2(format!("{release_id}:{name}"))
            .msg(&AssetRec {
                asset: Some(asset),
                key: key.clone(),
            });
        tx.insert(&ASSETS, &row).await?;
        tx.commit().await?;
        Ok(UploadTicket {
            asset_id: id,
            url: self.signer.upload(&key, size, content_type, now),
        })
    }

    async fn asset_locked(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(AssetRec, Row)> {
        match tx.get_for_update(&ASSETS, repo_id, id).await? {
            Some(row) => Ok((row.decode()?, row)),
            None => Err(ForgeError::not_found(format!("release asset {id}"))),
        }
    }

    /// Completes an upload with the SHA-256 of the bytes.
    pub async fn finalize_asset(
        &self,
        cx: &Cx,
        repo: &Repo,
        asset_id: &str,
        sha256: &[u8],
    ) -> Result<ReleaseAsset> {
        if sha256.len() != 32 {
            return Err(ForgeError::invalid("bad_digest", "sha256 must be 32 bytes"));
        }
        let release_id = {
            let mut lookup = self.db.begin(SURFACE, cx).await?;
            let (asset, _) = lookup
                .get_msg::<AssetRec>(&ASSETS, &repo.id, asset_id)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("release asset {asset_id}")))?;
            asset.asset.unwrap_or_default().release_id
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rrec, _) = Self::load_locked(&mut tx, &repo.id, &release_id).await?;
        Self::mutable_assets(&rrec)?;
        let (mut rec, mut row) = Self::asset_locked(&mut tx, &repo.id, asset_id).await?;
        let mut a = rec.asset.clone().unwrap_or_default();
        if a.release_id != release_id {
            return Err(ForgeError::not_found(format!("release asset {asset_id}")));
        }
        if a.state == ReleaseAssetState::Uploaded as i32 {
            return Err(ForgeError::invalid_state("the asset is already finalized"));
        }
        a.sha256 = sha256.to_vec();
        a.state = ReleaseAssetState::Uploaded as i32;
        a.updated_at = Some(ts(tx.now_ms()));
        rec.asset = Some(a.clone());
        row.body = rec.encode_to_vec();
        tx.update(&ASSETS, &row).await?;
        let payload =
            serde_json::json!({"release_id": a.release_id, "asset": &a, "blob_id": hex(sha256)});
        emit(
            &mut tx,
            "release.asset_uploaded",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Release, &repo.id, &a.release_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "release.asset_upload",
            &repo.org_id,
            &repo.id,
            SubjectKind::Release,
            &a.release_id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// Assets of a release (visible to those who see the release).
    pub async fn list_assets(
        &self,
        cx: &Cx,
        repo: &Repo,
        release_id: &str,
        see_drafts: bool,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<ReleaseAsset>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, release_id).await?;
        Self::hide_draft(&rec, see_drafts)?;
        let assets = Self::assets_of(&mut tx, &repo.id, release_id).await?;
        page_vec(assets, token, limit)
    }

    /// One asset.
    pub async fn get_asset(
        &self,
        cx: &Cx,
        repo: &Repo,
        asset_id: &str,
        see_drafts: bool,
    ) -> Result<ReleaseAsset> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = tx
            .get_msg::<AssetRec>(&ASSETS, &repo.id, asset_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("release asset {asset_id}")))?;
        let a = rec.asset.unwrap_or_default();
        let (rrec, _) = Self::load(&mut tx, &repo.id, &a.release_id).await?;
        Self::hide_draft(&rrec, see_drafts)?;
        Ok(a)
    }

    /// Renames or relabels an asset.
    pub async fn update_asset(
        &self,
        cx: &Cx,
        repo: &Repo,
        asset_id: &str,
        name: Option<&str>,
        label: Option<&str>,
    ) -> Result<ReleaseAsset> {
        let release_id = {
            let mut lookup = self.db.begin(SURFACE, cx).await?;
            let (asset, _) = lookup
                .get_msg::<AssetRec>(&ASSETS, &repo.id, asset_id)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("release asset {asset_id}")))?;
            asset.asset.unwrap_or_default().release_id
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rrec, _) = Self::load_locked(&mut tx, &repo.id, &release_id).await?;
        Self::mutable_assets(&rrec)?;
        let (mut rec, mut row) = Self::asset_locked(&mut tx, &repo.id, asset_id).await?;
        let mut a = rec.asset.clone().unwrap_or_default();
        if a.release_id != release_id {
            return Err(ForgeError::not_found(format!("release asset {asset_id}")));
        }
        if let Some(n) = name {
            required("name", n)?;
            max_len("name", n, 255)?;
            if n.contains('/') {
                return Err(ForgeError::invalid(
                    "invalid_asset_name",
                    "asset names cannot contain `/`",
                ));
            }
            a.name = n.to_string();
            row.k2 = Some(format!("{}:{n}", a.release_id));
        }
        if let Some(l) = label {
            max_len("label", l, 255)?;
            a.label = l.to_string();
        }
        a.updated_at = Some(ts(tx.now_ms()));
        rec.asset = Some(a.clone());
        row.body = rec.encode_to_vec();
        tx.update(&ASSETS, &row).await?;
        tx.audit(draft(
            "release.asset_update",
            &repo.org_id,
            &repo.id,
            SubjectKind::Release,
            &a.release_id,
            &serde_json::json!({"asset": a.id, "name": a.name}),
        ))
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// Deletes an asset.
    pub async fn delete_asset(&self, cx: &Cx, repo: &Repo, asset_id: &str) -> Result<()> {
        let release_id = {
            let mut lookup = self.db.begin(SURFACE, cx).await?;
            let (asset, _) = lookup
                .get_msg::<AssetRec>(&ASSETS, &repo.id, asset_id)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("release asset {asset_id}")))?;
            asset.asset.unwrap_or_default().release_id
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rrec, _) = Self::load_locked(&mut tx, &repo.id, &release_id).await?;
        Self::mutable_assets(&rrec)?;
        let (rec, _) = Self::asset_locked(&mut tx, &repo.id, asset_id).await?;
        let a = rec.asset.unwrap_or_default();
        if a.release_id != release_id {
            return Err(ForgeError::not_found(format!("release asset {asset_id}")));
        }
        tx.delete(&ASSETS, &repo.id, asset_id).await?;
        let payload =
            serde_json::json!({"release_id": a.release_id, "asset_id": a.id, "name": a.name});
        emit(
            &mut tx,
            "release.asset_deleted",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Release, &repo.id, &a.release_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "release.asset_delete",
            &repo.org_id,
            &repo.id,
            SubjectKind::Release,
            &a.release_id,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    /// A signed download URL; counts the download. Returns the URL, expiry, digest and size.
    pub async fn download_url(
        &self,
        cx: &Cx,
        repo: &Repo,
        asset_id: &str,
        see_drafts: bool,
    ) -> Result<(SignedUrl, Vec<u8>, u64)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::asset_locked(&mut tx, &repo.id, asset_id).await?;
        let mut a = rec.asset.clone().unwrap_or_default();
        let (rrec, _) = Self::load(&mut tx, &repo.id, &a.release_id).await?;
        Self::hide_draft(&rrec, see_drafts)?;
        if a.state != ReleaseAssetState::Uploaded as i32 {
            return Err(ForgeError::invalid_state("the asset is not uploaded yet")
                .with_subcode("asset_uploading"));
        }
        a.download_count += 1;
        rec.asset = Some(a.clone());
        row.body = rec.encode_to_vec();
        tx.update(&ASSETS, &row).await?;
        tx.commit().await?;
        Ok((
            self.signer.download(&rec.key, self.db.now_ms()),
            a.sha256,
            a.size,
        ))
    }

    // ---- attestations ------------------------------------------------------------------

    /// Records the attestation a signing worker produced for a published release.
    pub async fn record_attestation(
        &self,
        cx: &Cx,
        repo: &Repo,
        release_id: &str,
        bundle: &str,
    ) -> Result<ReleaseAttestation> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, release_id).await?;
        let r = rec.release.clone().unwrap_or_default();
        if r.draft {
            return Err(ForgeError::invalid_state("drafts are not attested"));
        }
        let assets = Self::assets_of(&mut tx, &repo.id, release_id).await?;
        let att = ReleaseAttestation {
            release_id: release_id.to_string(),
            tag_name: r.tag_name.clone(),
            target: r.target.clone(),
            subjects: assets
                .iter()
                .filter(|a| a.state == ReleaseAssetState::Uploaded as i32)
                .map(|a| bee_proto::v1::ReleaseAttestationSubject {
                    name: a.name.clone(),
                    sha256: a.sha256.clone(),
                })
                .collect(),
            bundle: bundle.to_string(),
            created_at: Some(ts(tx.now_ms())),
        };
        tx.put(&ATTESTATIONS, &Row::new(&repo.id, release_id).msg(&att))
            .await?;
        let payload = serde_json::json!({"release_id": release_id, "subjects": att.subjects.len()});
        emit(
            &mut tx,
            "release.attested",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Release, &repo.id, release_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "release.attest",
            &repo.org_id,
            &repo.id,
            SubjectKind::Release,
            release_id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(att)
    }

    /// The attestation of a release; `NOT_FOUND` / `attestation_pending` until recorded.
    pub async fn attestation(
        &self,
        cx: &Cx,
        repo: &Repo,
        release_id: &str,
        see_drafts: bool,
    ) -> Result<ReleaseAttestation> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, release_id).await?;
        Self::hide_draft(&rec, see_drafts)?;
        match tx
            .get_msg::<ReleaseAttestation>(&ATTESTATIONS, &repo.id, release_id)
            .await?
        {
            Some((a, _)) => Ok(a),
            None => {
                Err(ForgeError::not_found("release attestation")
                    .with_subcode("attestation_pending"))
            }
        }
    }

    // ---- source archives ---------------------------------------------------------------

    /// Records (once per tree, format and prefix) a source archive request and returns its
    /// signed URL, the resolved changeset and the tree id.
    pub async fn archive(
        &self,
        cx: &Cx,
        repo: &Repo,
        at: &str,
        format: SourceArchiveFormat,
        prefix: &str,
    ) -> Result<(SignedUrl, ObjectId, ObjectId)> {
        let format = if format == SourceArchiveFormat::Unspecified {
            SourceArchiveFormat::TarGz
        } else {
            format
        };
        let changeset = self.resolve(repo, at).await?;
        let vr = VcsRepo::of_repo(repo);
        let tree = self.vcs.tree_id(&vr, &changeset).await?;
        let prefix = if prefix.is_empty() {
            format!(
                "{}-{}",
                repo.name,
                &hex(&changeset.raw)[..12.min(hex(&changeset.raw).len())]
            )
        } else {
            prefix.to_string()
        };
        if prefix.contains('/') || prefix.contains("..") || prefix.chars().any(|c| c.is_control()) {
            return Err(ForgeError::invalid(
                "invalid_prefix",
                "the archive prefix is a single directory name",
            ));
        }
        let ext = if format == SourceArchiveFormat::Zip {
            "zip"
        } else {
            "tar.gz"
        };
        let request_key = format!("{}:{}:{prefix}", format as i32, hex(&tree.raw));
        let key = format!("archives/{}/{}/{prefix}.{ext}", repo.id, hex(&tree.raw));
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan(
                &ARCHIVES,
                &Scan::scope(&repo.id, 1).key(Key::K1(request_key.clone())),
            )
            .await?;
        match rows.into_iter().next() {
            Some(mut row) => {
                let mut rec: ArchiveRec = row.decode()?;
                rec.requests += 1;
                row.body = rec.encode_to_vec();
                tx.update(&ARCHIVES, &row).await?;
            }
            None => {
                let id = tx.new_id()?;
                let rec = ArchiveRec {
                    id: id.clone(),
                    repo_id: repo.id.clone(),
                    format: format as i32,
                    prefix: prefix.clone(),
                    changeset: changeset.raw.clone(),
                    tree: tree.raw.clone(),
                    key: key.clone(),
                    requested_by: cx.actor_id.clone(),
                    requests: 1,
                    built: false,
                };
                tx.insert(
                    &ARCHIVES,
                    &Row::new(&repo.id, id.clone())
                        .k1(request_key)
                        .n1(now as i64)
                        .msg(&rec),
                )
                .await?;
                let payload = serde_json::json!({"request_id": id, "tree": hex(&tree.raw), "format": ext, "prefix": prefix});
                emit(
                    &mut tx,
                    "release.archive_requested",
                    &repo.org_id,
                    &repo.id,
                    subject(SubjectKind::Repo, &repo.id, &repo.id),
                    payload.clone(),
                )
                .await?;
                tx.audit(draft(
                    "release.archive_request",
                    &repo.org_id,
                    &repo.id,
                    SubjectKind::Repo,
                    &repo.id,
                    &payload,
                ))
                .await?;
            }
        }
        tx.commit().await?;
        Ok((self.signer.download(&key, now), changeset, tree))
    }

    /// Archive requests the worker has not built yet, oldest first.
    pub async fn pending_archives(&self, cx: &Cx, limit: usize) -> Result<Vec<ArchiveRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<ArchiveRec>(
                &ARCHIVES,
                &Scan {
                    scope: None,
                    order: crate::store::Order::N1,
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(r, _)| r)
            .filter(|r| !r.built)
            .take(limit)
            .collect())
    }

    /// The archive request whose storage key is `key` (`archives/<repo>/<tree>/<prefix>.<ext>`),
    /// if one was recorded: the archive server builds only what was requested.
    pub async fn archive_request(
        &self,
        cx: &Cx,
        repo_id: &str,
        key: &str,
    ) -> Result<Option<ArchiveRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<ArchiveRec>(&ARCHIVES, &Scan::scope(repo_id, crate::store::MAX_SCAN))
            .await?;
        Ok(rows.into_iter().map(|(r, _)| r).find(|r| r.key == key))
    }

    /// Marks an archive request built.
    pub async fn mark_archive_built(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(mut row) = tx.get_for_update(&ARCHIVES, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("archive request {id}")));
        };
        let mut rec: ArchiveRec = row.decode()?;
        rec.built = true;
        row.body = rec.encode_to_vec();
        tx.update(&ARCHIVES, &row).await?;
        tx.commit().await
    }

    // ---- tags --------------------------------------------------------------------------

    fn tag_view(t: TagInfo, release_id: &str) -> RepoTag {
        RepoTag {
            name: t.name,
            target: Some(t.target),
            ref_version: t.ref_version,
            annotated: t.annotation.is_some(),
            annotation: t.annotation.map(|a| RepoTagAnnotation {
                id: Some(a.id),
                tagger: Some(a.tagger),
                tagger_time: Some(Signature {
                    name: String::new(),
                    time: Some(ts(a.time_ms)),
                    tz_offset_minutes: 0,
                }),
                message: a.message,
                signature: a.signature,
                verified: a.verified,
                verification_reason: a.reason,
            }),
            protected: t.protected,
            release_id: release_id.to_string(),
        }
    }

    async fn release_by_tag(&self, cx: &Cx, repo_id: &str) -> Result<BTreeMap<String, String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(&RELEASES, &Scan::scope(repo_id, MAX_RELEASES))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.k1?, r.id)))
            .collect())
    }

    /// Tags with a name prefix, sorted by name, each with the release that uses it.
    pub async fn list_tags(
        &self,
        cx: &Cx,
        repo: &Repo,
        prefix: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<RepoTag>, String)> {
        let tags = self.tags.list(&repo.id, prefix).await?;
        let by_tag = self.release_by_tag(cx, &repo.id).await?;
        let views: Vec<RepoTag> = tags
            .into_iter()
            .map(|t| {
                let rid = by_tag.get(&t.name).cloned().unwrap_or_default();
                Self::tag_view(t, &rid)
            })
            .collect();
        page_vec(views, token, limit)
    }

    /// One tag.
    pub async fn get_tag(&self, cx: &Cx, repo: &Repo, name: &str) -> Result<RepoTag> {
        let t = self
            .tags
            .get(&repo.id, name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("tag {name}")))?;
        let by_tag = self.release_by_tag(cx, &repo.id).await?;
        let rid = by_tag.get(name).cloned().unwrap_or_default();
        Ok(Self::tag_view(t, &rid))
    }

    /// Creates a tag; a non-empty `message` makes it annotated.
    pub async fn create_tag(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
        target: &str,
        message: &str,
        signature: &[u8],
    ) -> Result<RepoTag> {
        valid_tag(name)?;
        max_len("message", message, MAX_BODY)?;
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if tx.get(&TOMBSTONES, &repo.id, name).await?.is_some() {
                return Err(
                    ForgeError::already_exists(format!("tag {name}")).with_subcode("tag_immutable")
                );
            }
        }
        let target = self.resolve(repo, target).await?;
        let annotation = (!message.trim().is_empty()).then(|| NewAnnotation {
            tagger: cx.principal(),
            message: message.to_string(),
            signature: signature.to_vec(),
            time_ms: self.db.now_ms(),
        });
        if annotation.is_none() && !signature.is_empty() {
            return Err(ForgeError::invalid(
                "signature_needs_message",
                "a signature needs an annotated tag; give a message",
            ));
        }
        let t = self
            .tags
            .create(&repo.id, name, &target, annotation)
            .await?;
        self.record_tag(cx, repo, "tag.created", "tag.create", name)
            .await?;
        Ok(Self::tag_view(t, ""))
    }

    /// Deletes a tag that no published release uses.
    pub async fn delete_tag(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
        expected_version: u64,
    ) -> Result<()> {
        let by_tag = self.release_by_tag(cx, &repo.id).await?;
        if let Some(rid) = by_tag.get(name) {
            let r = self.get(cx, repo, rid, true).await?;
            if !r.draft {
                return Err(ForgeError::invalid_state(
                    "a published release uses this tag; delete the release first",
                )
                .with_subcode("tag_in_use"));
            }
        }
        self.tags.delete(&repo.id, name, expected_version).await?;
        self.record_tag(cx, repo, "tag.deleted", "tag.delete", name)
            .await
    }

    async fn record_tag(
        &self,
        cx: &Cx,
        repo: &Repo,
        kind: &str,
        audit: &str,
        name: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let payload = serde_json::json!({"ref_type": "tag", "ref": name});
        emit(
            &mut tx,
            kind,
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            audit,
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    /// Number of releases published in `[from_ms, to_ms)`.
    pub async fn published_between(
        &self,
        cx: &Cx,
        repo_id: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<u32> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all = Self::all(&mut tx, repo_id).await?;
        Ok(all
            .iter()
            .filter(|(r, _)| {
                r.release.as_ref().is_some_and(|x| {
                    !x.draft
                        && x.published_at
                            .as_ref()
                            .map(crate::ids::ts_ms)
                            .is_some_and(|t| t >= from_ms && t < to_ms)
                })
            })
            .count() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cr(n: u64, title: &str, who: &str, labels: &[&str]) -> LandedChange {
        LandedChange {
            id: format!("cr{n}"),
            number: n,
            title: title.into(),
            author: Principal {
                id: format!("id-{who}"),
                display: who.into(),
                ..Default::default()
            },
            labels: labels.iter().map(|s| s.to_string()).collect(),
            landed_at_ms: n,
        }
    }

    #[test]
    fn tag_names() {
        assert!(valid_tag("v1.2.3").is_ok());
        assert!(valid_tag("release/2026-01").is_ok());
        for bad in ["", "a b", "a..b", ".hidden", "x/", "v1.lock", "a~1", "-v"] {
            assert!(valid_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_notes_layout() {
        let changes = vec![cr(1, "Add x", "ann", &[]), cr(2, "Fix y", "bob", &["bug"])];
        let earlier: BTreeSet<String> = ["id-ann".to_string()].into();
        let (body, refs, numbers) = render_notes(
            &NotesConfig::default(),
            &changes,
            Some(&earlier),
            Some(("https://h/o/r", "v1", "v2")),
        );
        assert_eq!(
            body,
            "## What's Changed\n* Add x by @ann in #1\n* Fix y by @bob in #2\n\n## New Contributors\n* @bob made their first contribution in #2\n\n**Full Changelog**: https://h/o/r/compare/v1...v2\n"
        );
        assert_eq!(refs.len(), 2);
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    fn categories_and_exclusions() {
        let cfg = parse_notes_config("changelog:\n  exclude:\n    labels: [skip]\n    authors: [robot]\n  categories:\n    - title: Fixes\n      labels: [bug]\n    - title: Everything else\n      labels: ['*']\n").unwrap();
        let changes = vec![
            cr(1, "Add x", "ann", &[]),
            cr(2, "Fix y", "bob", &["bug"]),
            cr(3, "Bump", "robot", &[]),
            cr(4, "Chore", "ann", &["skip"]),
        ];
        let (body, _, numbers) = render_notes(&cfg, &changes, None, None);
        assert_eq!(
            body,
            "## Fixes\n* Fix y by @bob in #2\n\n## Everything else\n* Add x by @ann in #1\n"
        );
        assert_eq!(numbers, vec![1, 2]);
    }
}
