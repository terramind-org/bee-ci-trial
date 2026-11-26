//! [`Briefs`]: evidence of change requests (adding it inline, from a workflow artifact or
//! without files; the pending upload flow; reads, updates and deletes), the checklist, and the
//! brief as `BriefService` writes it.
//!
//! Callers authorize first (`service.rs`): these methods check evidence's own rules (kinds,
//! roles, sizes, the CI source rules, states) but not repo roles. One method is one
//! transaction on the `briefs` surface; bytes are stored in the CI blob store before it and
//! deleted again when it fails.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    ArtifactEvidenceAsset, BriefChecklist, ChangeBrief, ChangeRequest, ErrorReason, Event,
    Evidence, EvidenceAltText, EvidenceAsset, EvidenceAssetDigest, EvidenceAssetRole, EvidenceKind,
    EvidenceSource, EvidenceSpec, EvidenceState, EvidenceUploadAsset, InlineEvidenceAsset,
    ObjectId, Principal, PrincipalKind, Repo, RepoRef, SubjectKind, SubjectRef, WorkflowArtifact,
    repo_ref,
};

use super::checklist::{self, PathIn};
use super::model::{AssetRec, EvidenceRec};
use super::rules::{self, AssetIn};
use super::{EVIDENCE, PENDING_TTL_MS, SURFACE};
use crate::audit::AuditDraft;
use crate::changes::Changes;
use crate::checks::Checks;
use crate::ci::ports::{CiBlobs, MemoryCiBlobs, UrlSigner};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::hooks::PrincipalDirectory;
use crate::ids::{ts, unhex};
use crate::media::{
    EVIDENCE_BYTES_PER_CHANGE, EVIDENCE_INLINE_BYTES, EVIDENCE_ITEMS_PER_CHANGE, Kind, inspect,
    sniff,
};
use crate::store::{Db, Key, Row, Scan, Tx};

/// Where evidence files copied from workflow artifacts come from.
#[async_trait]
pub trait ArtifactSource: Send + Sync {
    /// An artifact of `repo`; `NOT_FOUND` when absent.
    async fn artifact(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<WorkflowArtifact>;
    /// The bytes of a finalized artifact.
    async fn artifact_bytes(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Vec<u8>>;
}

#[async_trait]
impl ArtifactSource for crate::ci::Ci {
    async fn artifact(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<WorkflowArtifact> {
        self.get_artifact(cx, repo, id).await
    }

    async fn artifact_bytes(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Vec<u8>> {
        crate::ci::Ci::artifact_bytes(self, cx, &repo.id, id).await
    }
}

/// What the briefs module needs beyond the forge and the change requests.
#[derive(Clone)]
pub struct BriefPorts {
    /// Evidence bytes (in the repo's org namespace).
    pub blobs: Arc<dyn CiBlobs>,
    /// Signs the `/_evidence/...` upload and download URLs.
    pub urls: UrlSigner,
    /// Workflow artifacts (`None`: evidence from artifacts is `UNIMPLEMENTED`).
    pub artifacts: Option<Arc<dyn ArtifactSource>>,
}

impl Default for BriefPorts {
    fn default() -> BriefPorts {
        let mut key = [0u8; 32];
        let _ = getrandom::fill(&mut key);
        BriefPorts {
            blobs: Arc::new(MemoryCiBlobs::new()),
            urls: UrlSigner::new("https://hub.invalid", &key),
            artifacts: None,
        }
    }
}

/// Where the files of a new evidence item come from.
#[derive(Clone, Debug, Default)]
pub enum Files {
    /// No files (a benchmark, a preview link, a test report without its document).
    #[default]
    None,
    /// Sent with the request.
    Inline(Vec<InlineEvidenceAsset>),
    /// Copied out of a workflow artifact.
    Artifact {
        /// The artifact.
        id: String,
        /// The files to take from it.
        assets: Vec<ArtifactEvidenceAsset>,
    },
}

/// A new evidence item.
#[derive(Clone, Debug, Default)]
pub struct NewEvidence {
    /// What it shows.
    pub spec: EvidenceSpec,
    /// The revision it was captured for (0 = current; with a check run, the tested one).
    pub revision: u32,
    /// The check run the caller reports, for CI evidence.
    pub check_run_id: String,
    /// Its files.
    pub files: Files,
}

/// Where an item comes from: its revision, that revision's head, and its source.
struct Origin {
    revision: u32,
    head: Option<ObjectId>,
    source: EvidenceSource,
    check_run_id: String,
    artifact_id: String,
}

/// Bytes checked and ready to store for one asset.
struct Checked {
    asset: EvidenceAsset,
    sha256: String,
    bytes: Vec<u8>,
}

/// Briefs and evidence over the forge store.
pub struct Briefs {
    pub(crate) db: Db,
    pub(crate) changes: Arc<Changes>,
    pub(crate) checks: Arc<Checks>,
    pub(crate) directory: Arc<dyn PrincipalDirectory>,
    pub(crate) blobs: Arc<dyn CiBlobs>,
    pub(crate) urls: UrlSigner,
    pub(crate) artifacts: Option<Arc<dyn ArtifactSource>>,
}

fn bad(message: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_evidence", message)
}

fn role_of(r: i32) -> EvidenceAssetRole {
    EvidenceAssetRole::try_from(r).unwrap_or(EvidenceAssetRole::Unspecified)
}

/// The row of an item: `n1` (its expiry) only while it is pending, so upkeep scans pending
/// items alone.
fn row_of(rec: &EvidenceRec, version: u64) -> Row {
    let e = rec.evidence();
    let mut row = Row::new(e.repo_id.clone(), e.id.clone())
        .k1(e.change_request_id.clone())
        .version(version)
        .msg(rec);
    if rec.state() == EvidenceState::Pending {
        row.n1 = Some(rec.expires_ms as i64);
    }
    row
}

fn evidence_json(e: &Evidence) -> String {
    serde_json::to_string(e).unwrap_or_default()
}

/// The order of a change request's items: hero first, then `spec.order`, then creation.
fn gallery_key(e: &Evidence) -> (bool, i32, String) {
    let s = e.spec.clone().unwrap_or_default();
    (!s.hero, s.order, e.id.clone())
}

/// The revision whose head is `head` (the newest one, should a head repeat).
fn revision_with_head(cr: &ChangeRequest, head: &ObjectId) -> Option<u32> {
    cr.revisions
        .iter()
        .filter(|r| r.head.as_ref() == Some(head))
        .map(|r| r.n)
        .max()
}

/// Pairs `values` (role, value) with `assets` in order within each role: the k-th value of a
/// role belongs to the k-th asset of that role. `None` for an asset without one; values left
/// over are an error.
fn by_role<T: Clone>(
    assets: &[EvidenceAsset],
    values: &[(i32, T)],
    what: &str,
) -> Result<Vec<Option<T>>> {
    let mut used = vec![false; values.len()];
    let mut out = Vec::with_capacity(assets.len());
    for a in assets {
        let hit = values
            .iter()
            .enumerate()
            .find(|(i, (r, _))| !used[*i] && *r == a.role);
        match hit {
            Some((i, (_, v))) => {
                used[i] = true;
                out.push(Some(v.clone()));
            }
            None => out.push(None),
        }
    }
    if let Some(i) = used.iter().position(|u| !u) {
        return Err(bad(format!(
            "{what}[{i}] names a {} asset the item does not have",
            role_of(values[i].0).as_str_name()
        )));
    }
    Ok(out)
}

impl Briefs {
    /// The briefs module over `forge`'s store, checks and directory, the change requests,
    /// and `ports`.
    pub fn new(forge: &Forge, changes: Arc<Changes>, ports: BriefPorts) -> Arc<Briefs> {
        Arc::new(Briefs {
            db: forge.db.clone(),
            changes,
            checks: forge.checks.clone(),
            directory: forge.hooks.directory.clone(),
            blobs: ports.blobs,
            urls: ports.urls,
            artifacts: ports.artifacts,
        })
    }

    /// The change requests.
    pub fn changes(&self) -> &Arc<Changes> {
        &self.changes
    }

    /// The URL signer.
    pub fn urls(&self) -> &UrlSigner {
        &self.urls
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

    // ----- the brief and the checklist ---------------------------------------------------

    /// Sets a CR's brief ([`Changes::set_brief`]) and returns the CR with the checklist of its
    /// current revision.
    pub async fn set_brief(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        brief: &ChangeBrief,
        expected_version: u64,
    ) -> Result<(ChangeRequest, BriefChecklist)> {
        let cr = self
            .changes
            .set_brief(cx, repo, change_id, brief, expected_version)
            .await?;
        let list = self.checklist(cx, repo, &cr.id, 0).await?;
        Ok((cr, list))
    }

    /// The checklist of revision `revision` (0 = current) of a CR.
    pub async fn checklist(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        revision: u32,
    ) -> Result<BriefChecklist> {
        let (cr, n, paths) = self
            .changes
            .revision_paths(cx, repo, change_id, revision)
            .await?;
        let evidence: Vec<Evidence> = self
            .items(cx, &repo.id, &cr.id)
            .await?
            .into_iter()
            .map(|(r, _)| r.evidence().clone())
            .collect();
        let author_kind = cr
            .author
            .as_ref()
            .and_then(|a| PrincipalKind::try_from(a.kind).ok())
            .unwrap_or(PrincipalKind::Unspecified);
        let ins: Vec<PathIn> = paths
            .iter()
            .map(|p| PathIn {
                path: &p.path,
                class: &p.class,
            })
            .collect();
        Ok(checklist::compute(&checklist::Inputs {
            revision: n,
            author_kind,
            paths: &ins,
            brief: cr.brief.as_deref(),
            evidence: &evidence,
        }))
    }

    // ----- reads -------------------------------------------------------------------------

    /// Every item of a CR, any state, in gallery order.
    async fn items(
        &self,
        cx: &Cx,
        repo_id: &str,
        change_id: &str,
    ) -> Result<Vec<(EvidenceRec, Row)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::items_in(&mut tx, repo_id, change_id).await
    }

    async fn items_in(
        tx: &mut Tx,
        repo_id: &str,
        change_id: &str,
    ) -> Result<Vec<(EvidenceRec, Row)>> {
        let scan = Scan::scope(repo_id.to_string(), 10_000).key(Key::K1(change_id.to_string()));
        let mut v = tx.scan_msgs::<EvidenceRec>(&EVIDENCE, &scan).await?;
        v.sort_by_key(|(r, _)| gallery_key(r.evidence()));
        Ok(v)
    }

    async fn load(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(EvidenceRec, Row)> {
        tx.get_msg::<EvidenceRec>(&EVIDENCE, repo_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("evidence {id}")))
    }

    /// One item, any state.
    pub async fn get(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<EvidenceRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::load(&mut tx, &repo.id, id).await?.0)
    }

    /// Ready items of `change_ids` (in that order, each once), each CR's in gallery order;
    /// with `hero_only`, at most the first of each.
    pub async fn list(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_ids: &[String],
        hero_only: bool,
    ) -> Result<Vec<EvidenceRec>> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for id in change_ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            let ready = Self::items_in(&mut tx, &repo.id, id)
                .await?
                .into_iter()
                .filter(|(r, _)| r.state() == EvidenceState::Ready)
                .map(|(r, _)| r);
            if hero_only {
                out.extend(ready.take(1));
            } else {
                out.extend(ready);
            }
        }
        Ok(out)
    }

    /// The item as `chain` is served it: signed URLs for ready assets (valid from `now_ms`),
    /// and stale when `current_revision` is newer than the item's.
    pub fn served(
        &self,
        rec: &EvidenceRec,
        chain: &[crate::media::ChainLink],
        now_ms: u64,
        current_revision: u32,
    ) -> Evidence {
        let mut e = rec.evidence().clone();
        e.stale = e.revision < current_revision;
        if rec.state() == EvidenceState::Ready {
            for (i, a) in e.assets.iter_mut().enumerate() {
                let (url, exp) = self.download_url(&e.repo_id, &e.id, i, chain, now_ms);
                a.url = url;
                a.url_expires_at = Some(ts(exp));
            }
        }
        e
    }

    // ----- writes ------------------------------------------------------------------------

    /// The revision, head and source of a new item (SPEC §11.9 "Sources"): a check run's
    /// evidence must come from its reporter, and the run's changeset must be the head of a
    /// revision of the CR, which becomes the item's; an artifact's changeset likewise.
    async fn origin(
        &self,
        cx: &Cx,
        repo: &Repo,
        cr: &ChangeRequest,
        revision: u32,
        check_run_id: &str,
        artifact: Option<&WorkflowArtifact>,
    ) -> Result<Origin> {
        let mut pinned: Option<u32> = None;
        let mut source = EvidenceSource::Uploaded;
        if !check_run_id.is_empty() {
            let run = self.checks.get(cx, &repo.id, check_run_id).await?;
            let reporter = run.reporter.as_ref().map(|p| p.id.as_str()).unwrap_or("");
            if reporter != cx.actor_id {
                return Err(ForgeError::permission_denied(format!(
                    "only the reporter of check run {check_run_id} may attach evidence for it"
                )));
            }
            let n = run
                .changeset
                .as_ref()
                .and_then(|h| revision_with_head(cr, h))
                .ok_or_else(|| {
                    bad(format!(
                        "check run {check_run_id} tested a changeset that is not the head of a \
                         revision of change request #{}",
                        cr.number
                    ))
                })?;
            pinned = Some(n);
            source = EvidenceSource::CheckRun;
        }
        if let Some(a) = artifact {
            let n = a
                .head_changeset
                .as_ref()
                .and_then(|h| revision_with_head(cr, h))
                .ok_or_else(|| {
                    bad(format!(
                        "artifact {} was built from a changeset that is not the head of a \
                         revision of change request #{}",
                        a.id, cr.number
                    ))
                })?;
            if pinned.is_some_and(|p| p != n) {
                return Err(bad(format!(
                    "artifact {} and check run {check_run_id} tested different revisions",
                    a.id
                )));
            }
            pinned = Some(n);
            source = EvidenceSource::WorkflowArtifact;
        }
        let current = cr.revisions.iter().map(|r| r.n).max().unwrap_or(0);
        let n = match (pinned, revision) {
            (Some(p), 0) => p,
            (Some(p), r) if r == p => p,
            (Some(p), r) => {
                return Err(bad(format!(
                    "revision {r} was requested, but the CI evidence is of revision {p}"
                )));
            }
            (None, 0) => current,
            (None, r) => r,
        };
        let head = cr
            .revisions
            .iter()
            .find(|r| r.n == n)
            .ok_or_else(|| bad(format!("change request #{} has no revision {n}", cr.number)))?
            .head
            .clone();
        Ok(Origin {
            revision: n,
            head,
            source,
            check_run_id: check_run_id.to_string(),
            artifact_id: artifact.map(|a| a.id.clone()).unwrap_or_default(),
        })
    }

    /// The skeleton of a new item.
    async fn draft(
        &self,
        cx: &Cx,
        repo: &Repo,
        cr: &ChangeRequest,
        spec: EvidenceSpec,
        origin: Origin,
        state: EvidenceState,
    ) -> Result<EvidenceRec> {
        let now = self.db.now_ms();
        Ok(EvidenceRec {
            evidence: Some(Evidence {
                id: self.db.ids().ulid()?,
                repo_id: repo.id.clone(),
                change_request_id: cr.id.clone(),
                revision: origin.revision,
                head: origin.head,
                spec: Some(spec),
                assets: Vec::new(),
                source: origin.source as i32,
                check_run_id: origin.check_run_id,
                workflow_artifact_id: origin.artifact_id,
                uploader: Some(self.actor(cx).await),
                state: state as i32,
                stale: false,
                created_at: Some(ts(now)),
                updated_at: Some(ts(now)),
            }),
            org_id: repo.org_id.clone(),
            assets: Vec::new(),
            expires_ms: if state == EvidenceState::Pending {
                now + PENDING_TTL_MS
            } else {
                0
            },
        })
    }

    /// Checks one file's bytes as `asset` declares them.
    fn check_bytes(
        kind: EvidenceKind,
        a: &AssetIn,
        media: Kind,
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        duration_ms: u64,
    ) -> Result<Checked> {
        let class = rules::class_of(kind, a.role)?;
        let found = inspect(class.limit(), Some(media), &bytes)?;
        let (w, h) = if found.kind.is_image() {
            (found.width, found.height)
        } else {
            (width, height)
        };
        Ok(Checked {
            asset: EvidenceAsset {
                role: a.role as i32,
                content_type: found.kind.content_type().to_string(),
                digest: format!("sha256:{}", found.sha256),
                size_bytes: found.size,
                width: w,
                height: h,
                duration_ms,
                alt: a.alt.clone(),
                url: String::new(),
                url_expires_at: None,
            },
            sha256: found.sha256,
            bytes,
        })
    }

    /// The files of an inline request, checked.
    fn inline_files(kind: EvidenceKind, assets: Vec<InlineEvidenceAsset>) -> Result<Vec<Checked>> {
        let total: u64 = assets.iter().map(|a| a.data.len() as u64).sum();
        if total > EVIDENCE_INLINE_BYTES {
            return Err(ForgeError::too_large(
                "evidence_inline_bytes",
                EVIDENCE_INLINE_BYTES,
                total,
            ));
        }
        let ins: Vec<AssetIn> = assets
            .iter()
            .map(|a| AssetIn {
                role: role_of(a.role),
                content_type: a.content_type.clone(),
                size: a.data.len() as u64,
                alt: a.alt.clone(),
            })
            .collect();
        let kinds = rules::check_assets(kind, &ins)?;
        assets
            .into_iter()
            .zip(ins.iter().zip(kinds))
            .map(|(a, (i, k))| {
                Self::check_bytes(kind, i, k, a.data, a.width, a.height, a.duration_ms)
            })
            .collect()
    }

    /// The files of an artifact request, copied out of the artifact and checked. Each file is
    /// declared by its signature, else (text) by its extension.
    async fn artifact_files(
        &self,
        cx: &Cx,
        repo: &Repo,
        kind: EvidenceKind,
        art: &WorkflowArtifact,
        assets: &[ArtifactEvidenceAsset],
    ) -> Result<Vec<Checked>> {
        let src = self.artifacts.as_ref().ok_or_else(|| {
            crate::service::unserved(
                "AddEvidence.workflow_artifact_id",
                "the workflow artifacts (crate::briefs::ArtifactSource)",
            )
        })?;
        let data = src.artifact_bytes(cx, repo, &art.id).await?;
        let entries = if art.archive {
            Some(crate::packages::archive::unpack_artifact(
                &data,
                EVIDENCE_BYTES_PER_CHANGE as usize,
            )?)
        } else {
            None
        };
        let mut files = Vec::with_capacity(assets.len());
        for (i, a) in assets.iter().enumerate() {
            let (name, bytes) = match &entries {
                Some(es) => {
                    let want = crate::packages::archive::clean_path(&a.path)
                        .ok_or_else(|| bad(format!("artifact_assets[{i}].path is not a path")))?;
                    let e = es
                        .iter()
                        .find(|e| e.path == want)
                        .ok_or_else(|| bad(format!("artifact {} has no file {want}", art.id)))?;
                    (want, e.data.clone())
                }
                None => {
                    if !a.path.is_empty() && a.path != art.name {
                        return Err(bad(format!(
                            "artifact {} is a single file; artifact_assets[{i}].path must be \
                             empty",
                            art.id
                        )));
                    }
                    (art.name.clone(), data.clone())
                }
            };
            let declared = sniff(&bytes)
                .or_else(|| {
                    (!art.archive)
                        .then(|| Kind::from_content_type(&art.content_type))
                        .flatten()
                        .filter(|k| k.is_text())
                })
                .or_else(|| rules::text_kind_of_path(&name));
            let content_type = declared
                .map(|k| k.content_type().to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            files.push((
                AssetIn {
                    role: role_of(a.role),
                    content_type,
                    size: bytes.len() as u64,
                    alt: a.alt.clone(),
                },
                bytes,
                a,
            ));
        }
        let ins: Vec<AssetIn> = files.iter().map(|(i, _, _)| i.clone()).collect();
        let kinds = rules::check_assets(kind, &ins)?;
        files
            .into_iter()
            .zip(kinds)
            .map(|((i, bytes, a), k)| {
                Self::check_bytes(kind, &i, k, bytes, a.width, a.height, a.duration_ms)
            })
            .collect()
    }

    /// Adds an item whose files come with the request, from an artifact, or that has none.
    pub async fn add(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        new: NewEvidence,
    ) -> Result<EvidenceRec> {
        let kind = rules::check_spec(&new.spec)?;
        let cr = self.changes.get(cx, repo, change_id).await?;
        let (checked, artifact) = match new.files {
            Files::None => {
                rules::check_assets(kind, &[])?;
                (Vec::new(), None)
            }
            Files::Inline(assets) => (Self::inline_files(kind, assets)?, None),
            Files::Artifact { id, assets } => {
                let src = self.artifacts.as_ref().ok_or_else(|| {
                    crate::service::unserved(
                        "AddEvidence.workflow_artifact_id",
                        "the workflow artifacts (crate::briefs::ArtifactSource)",
                    )
                })?;
                let art = src.artifact(cx, repo, &id).await?;
                if art.expired || !art.finalized {
                    return Err(ForgeError::invalid_state(format!(
                        "artifact {id} is expired or not finalized"
                    )));
                }
                let files = self.artifact_files(cx, repo, kind, &art, &assets).await?;
                (files, Some(art))
            }
        };
        let origin = self
            .origin(
                cx,
                repo,
                &cr,
                new.revision,
                &new.check_run_id,
                artifact.as_ref(),
            )
            .await?;
        let mut rec = self
            .draft(cx, repo, &cr, new.spec, origin, EvidenceState::Ready)
            .await?;
        let mut stored = Vec::new();
        for c in checked {
            let blob = match self.blobs.put(&repo.org_id, c.bytes).await {
                Ok(b) => b,
                Err(e) => {
                    self.discard(&repo.org_id, &stored).await;
                    return Err(e);
                }
            };
            stored.push(blob.clone());
            rec.assets.push(AssetRec {
                role: c.asset.role,
                blob,
                declared_type: c.asset.content_type.clone(),
                declared_size: c.asset.size_bytes,
                sha256: c.sha256,
                size: c.asset.size_bytes,
            });
            rec.evidence_mut().assets.push(c.asset);
        }
        match self.insert(cx, &rec, "evidence.add").await {
            Ok(()) => Ok(rec),
            Err(e) => {
                self.discard(&repo.org_id, &stored).await;
                Err(e)
            }
        }
    }

    /// Starts an upload: a pending item with one asset per declared file. Returns the item;
    /// the URLs are signed by [`Briefs::upload_url`] for each asset index.
    #[allow(clippy::too_many_arguments)]
    pub async fn begin(
        &self,
        cx: &Cx,
        repo: &Repo,
        change_id: &str,
        spec: EvidenceSpec,
        revision: u32,
        check_run_id: &str,
        assets: &[EvidenceUploadAsset],
    ) -> Result<EvidenceRec> {
        let kind = rules::check_spec(&spec)?;
        if assets.is_empty() {
            return Err(bad(
                "an upload needs at least one asset; add evidence without files with AddEvidence",
            ));
        }
        let ins: Vec<AssetIn> = assets
            .iter()
            .map(|a| AssetIn {
                role: role_of(a.role),
                content_type: a.content_type.clone(),
                size: a.size_bytes,
                alt: a.alt.clone(),
            })
            .collect();
        let kinds = rules::check_assets(kind, &ins)?;
        let cr = self.changes.get(cx, repo, change_id).await?;
        let origin = self
            .origin(cx, repo, &cr, revision, check_run_id, None)
            .await?;
        let mut rec = self
            .draft(cx, repo, &cr, spec, origin, EvidenceState::Pending)
            .await?;
        for (a, k) in assets.iter().zip(kinds) {
            rec.assets.push(AssetRec {
                role: a.role,
                blob: String::new(),
                declared_type: k.content_type().to_string(),
                declared_size: a.size_bytes,
                sha256: String::new(),
                size: 0,
            });
            rec.evidence_mut().assets.push(EvidenceAsset {
                role: a.role,
                content_type: k.content_type().to_string(),
                size_bytes: a.size_bytes,
                width: a.width,
                height: a.height,
                duration_ms: a.duration_ms,
                alt: a.alt.clone(),
                ..Default::default()
            });
        }
        self.insert(cx, &rec, "evidence.begin_upload").await?;
        Ok(rec)
    }

    /// Stores the bytes of asset `index` of a pending item (the upload URL's handler; any
    /// earlier bytes are replaced). `INVALID_STATE` once the item is ready or expired,
    /// `TOO_LARGE` above the asset's class limit, `bad_media` when the bytes are not the
    /// declared type.
    pub async fn receive(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
        index: usize,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let now = self.db.now_ms();
        let (rec, _) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::load(&mut tx, repo_id, id).await?
        };
        Self::receivable(&rec, now)?;
        let kind = rules::kind_of(&rec.spec())?;
        let a = rec
            .assets
            .get(index)
            .ok_or_else(|| ForgeError::not_found(format!("asset {index} of evidence {id}")))?;
        let media = Kind::from_content_type(&a.declared_type)
            .ok_or_else(|| ForgeError::internal(format!("stored type {}", a.declared_type)))?;
        let class = rules::class_of(kind, a.role())?;
        let found = inspect(class.limit(), Some(media), &bytes)?;
        let blob = self.blobs.put(&rec.org_id, bytes).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut cur, row) = match Self::load(&mut tx, repo_id, id).await {
            Ok(v) => v,
            Err(e) => {
                drop(tx);
                self.discard(&rec.org_id, &[blob]).await;
                return Err(e);
            }
        };
        if let Err(e) = Self::receivable(&cur, now) {
            drop(tx);
            self.discard(&rec.org_id, &[blob]).await;
            return Err(e);
        }
        let old = std::mem::replace(&mut cur.assets[index].blob, blob.clone());
        cur.assets[index].sha256 = found.sha256;
        cur.assets[index].size = found.size;
        let ea = &mut cur.evidence_mut().assets[index];
        ea.content_type = found.kind.content_type().to_string();
        if found.kind.is_image() {
            ea.width = found.width;
            ea.height = found.height;
        }
        let res = async {
            tx.update(&EVIDENCE, &row_of(&cur, row.version)).await?;
            tx.commit().await
        }
        .await;
        match res {
            Ok(()) => {
                if !old.is_empty() {
                    self.discard(&rec.org_id, &[old]).await;
                }
                Ok(())
            }
            Err(e) => {
                self.discard(&rec.org_id, &[blob]).await;
                Err(e)
            }
        }
    }

    fn receivable(rec: &EvidenceRec, now: u64) -> Result<()> {
        if rec.state() != EvidenceState::Pending {
            return Err(ForgeError::invalid_state(format!(
                "evidence {} is not pending",
                rec.evidence().id
            )));
        }
        if now >= rec.expires_ms {
            return Err(ForgeError::invalid_state(format!(
                "the upload of evidence {} expired",
                rec.evidence().id
            ))
            .with_subcode("upload_expired"));
        }
        Ok(())
    }

    /// Completes an upload: every asset received (`missing_asset`), each digest equal to the
    /// one the server computed and each size to the declared one (`INTEGRITY`). The item
    /// becomes ready and `evidence.added` is emitted. Digests pair with assets in order within
    /// each role.
    pub async fn finalize(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        digests: &[EvidenceAssetDigest],
    ) -> Result<EvidenceRec> {
        let now = self.db.now_ms();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        Self::receivable(&rec, now)?;
        let declared: Vec<(i32, String)> = digests
            .iter()
            .map(|d| (d.role, d.sha256.trim().to_ascii_lowercase()))
            .collect();
        for (i, (_, d)) in declared.iter().enumerate() {
            if d.len() != 64 || unhex(d).is_none() {
                return Err(ForgeError::invalid(
                    "bad_digest",
                    format!("digests[{i}].sha256 must be 64 hex characters"),
                ));
            }
        }
        let paired = by_role(&rec.evidence().assets, &declared, "digests")?;
        for (i, (a, d)) in rec.assets.iter().zip(&paired).enumerate() {
            if a.blob.is_empty() {
                return Err(ForgeError::invalid(
                    "missing_asset",
                    format!(
                        "asset {i} ({}) of evidence {id} was never uploaded",
                        a.role().as_str_name()
                    ),
                ));
            }
            let Some(d) = d else {
                return Err(ForgeError::invalid(
                    "bad_digest",
                    format!("no digest for asset {i} ({})", a.role().as_str_name()),
                ));
            };
            if *d != a.sha256 || a.size != a.declared_size {
                return Err(ForgeError::new(
                    ErrorReason::Integrity,
                    format!(
                        "asset {i}: received {} bytes with sha256 {}, declared {} bytes with \
                         sha256 {d}",
                        a.size, a.sha256, a.declared_size
                    ),
                ));
            }
        }
        let shas: Vec<String> = rec.assets.iter().map(|a| a.sha256.clone()).collect();
        let e = rec.evidence_mut();
        for (a, sha) in e.assets.iter_mut().zip(shas) {
            a.digest = format!("sha256:{sha}");
        }
        e.state = EvidenceState::Ready as i32;
        e.updated_at = Some(ts(now));
        rec.expires_ms = 0;
        Self::clear_other_heroes(&mut tx, &rec).await?;
        tx.update(&EVIDENCE, &row_of(&rec, row.version)).await?;
        Self::emit(&mut tx, "evidence.added", &rec).await?;
        tx.audit(Self::audit("evidence.finalize", &rec)).await?;
        tx.commit().await?;
        Ok(rec)
    }

    /// Replaces an item's spec (the kind never changes: `kind_immutable`) and the alt text of
    /// its assets (paired in order within each role), and emits `evidence.updated`.
    pub async fn update(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        spec: EvidenceSpec,
        alt_texts: &[EvidenceAltText],
    ) -> Result<EvidenceRec> {
        let kind = rules::check_spec(&spec)?;
        let now = self.db.now_ms();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        if rules::kind_of(&rec.spec())? != kind {
            return Err(ForgeError::invalid(
                "kind_immutable",
                "the kind of evidence never changes; add a new item instead",
            ));
        }
        let alts: Vec<(i32, String)> = alt_texts.iter().map(|a| (a.role, a.alt.clone())).collect();
        let paired = by_role(&rec.evidence().assets, &alts, "alt_texts")?;
        let e = rec.evidence_mut();
        for (a, alt) in e.assets.iter_mut().zip(paired) {
            if let Some(alt) = alt {
                a.alt = alt;
            }
        }
        let ins: Vec<AssetIn> = rec
            .evidence()
            .assets
            .iter()
            .map(|a| AssetIn {
                role: role_of(a.role),
                content_type: a.content_type.clone(),
                size: a.size_bytes,
                alt: a.alt.clone(),
            })
            .collect();
        rules::check_assets(kind, &ins)?;
        let e = rec.evidence_mut();
        e.spec = Some(spec);
        e.updated_at = Some(ts(now));
        if rec.state() == EvidenceState::Ready {
            Self::clear_other_heroes(&mut tx, &rec).await?;
        }
        tx.update(&EVIDENCE, &row_of(&rec, row.version)).await?;
        Self::emit(&mut tx, "evidence.updated", &rec).await?;
        tx.audit(Self::audit("evidence.update", &rec)).await?;
        tx.commit().await?;
        Ok(rec)
    }

    /// Deletes an item and its files, and emits `evidence.deleted` (a pending item was never
    /// added, so it emits nothing).
    pub async fn delete(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<EvidenceRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
        tx.delete(&EVIDENCE, &repo.id, id).await?;
        if rec.state() == EvidenceState::Ready {
            Self::emit(&mut tx, "evidence.deleted", &rec).await?;
        }
        tx.audit(Self::audit("evidence.delete", &rec)).await?;
        tx.commit().await?;
        let blobs: Vec<String> = rec
            .assets
            .iter()
            .filter(|a| !a.blob.is_empty())
            .map(|a| a.blob.clone())
            .collect();
        self.discard(&rec.org_id, &blobs).await;
        Ok(rec)
    }

    /// Inserts a new item after the per-CR bounds, clearing the CR's other hero when a ready
    /// item is one; a ready item emits `evidence.added`.
    async fn insert(&self, cx: &Cx, rec: &EvidenceRec, action: &str) -> Result<()> {
        let e = rec.evidence();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let existing = Self::items_in(&mut tx, &e.repo_id, &e.change_request_id).await?;
        let n = existing.len() as u64 + 1;
        if n > EVIDENCE_ITEMS_PER_CHANGE {
            return Err(ForgeError::too_large(
                "evidence_items",
                EVIDENCE_ITEMS_PER_CHANGE,
                n,
            ));
        }
        let bytes: u64 = existing.iter().map(|(r, _)| r.bytes()).sum::<u64>() + rec.bytes();
        if bytes > EVIDENCE_BYTES_PER_CHANGE {
            return Err(ForgeError::too_large(
                "evidence_bytes",
                EVIDENCE_BYTES_PER_CHANGE,
                bytes,
            ));
        }
        if rec.state() == EvidenceState::Ready {
            Self::clear_other_heroes(&mut tx, rec).await?;
        }
        tx.insert(&EVIDENCE, &row_of(rec, 1)).await?;
        if rec.state() == EvidenceState::Ready {
            Self::emit(&mut tx, "evidence.added", rec).await?;
        }
        tx.audit(Self::audit(action, rec)).await?;
        tx.commit().await
    }

    /// When `rec` is the hero, no other item of its CR is.
    async fn clear_other_heroes(tx: &mut Tx, rec: &EvidenceRec) -> Result<()> {
        if !rec.spec().hero {
            return Ok(());
        }
        let e = rec.evidence();
        for (mut other, row) in Self::items_in(tx, &e.repo_id, &e.change_request_id).await? {
            if other.evidence().id == e.id || !other.spec().hero {
                continue;
            }
            if let Some(s) = other.evidence_mut().spec.as_mut() {
                s.hero = false;
            }
            tx.update(&EVIDENCE, &row_of(&other, row.version)).await?;
        }
        Ok(())
    }

    async fn emit(tx: &mut Tx, kind: &str, rec: &EvidenceRec) -> Result<()> {
        let e = rec.evidence();
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: e.repo_id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(e.repo_id.clone())),
                }),
                id: e.change_request_id.clone(),
            }),
            payload_json: evidence_json(e),
            ..Default::default()
        })
        .await
    }

    fn audit(action: &str, rec: &EvidenceRec) -> AuditDraft {
        let e = rec.evidence();
        AuditDraft::new(action, &rec.org_id)
            .repo_id(&e.repo_id)
            .target(SubjectKind::ChangeRequest, &e.change_request_id)
            .after(&evidence_json(e))
    }

    /// Deletes stored blobs, best effort (a blob left behind holds no reference).
    pub(crate) async fn discard(&self, org_id: &str, blobs: &[String]) {
        for b in blobs {
            let _ = self.blobs.delete(org_id, b).await;
        }
    }

    /// The bytes of a stored asset.
    pub(crate) async fn asset_bytes(&self, rec: &EvidenceRec, index: usize) -> Result<Vec<u8>> {
        let a = rec.assets.get(index).ok_or_else(|| {
            ForgeError::not_found(format!("asset {index} of evidence {}", rec.evidence().id))
        })?;
        self.blobs
            .get(&rec.org_id, &a.blob)
            .await?
            .ok_or_else(|| ForgeError::new(ErrorReason::MissingObject, "evidence bytes missing"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(role: EvidenceAssetRole) -> EvidenceAsset {
        EvidenceAsset {
            role: role as i32,
            ..Default::default()
        }
    }

    #[test]
    fn values_pair_with_assets_in_order_within_a_role() {
        use EvidenceAssetRole as R;
        let assets = [asset(R::Main), asset(R::Poster), asset(R::Main)];
        let got = by_role(
            &assets,
            &[
                (R::Main as i32, 1),
                (R::Main as i32, 2),
                (R::Poster as i32, 3),
            ],
            "x",
        )
        .unwrap();
        assert_eq!(got, vec![Some(1), Some(3), Some(2)]);
        let got = by_role(&assets, &[(R::Main as i32, 1)], "x").unwrap();
        assert_eq!(got, vec![Some(1), None, None]);
        let e = by_role(&assets, &[(R::After as i32, 1)], "x").unwrap_err();
        assert_eq!(e.subcode, "bad_evidence");
    }

    #[test]
    fn gallery_puts_the_hero_first_then_order_then_age() {
        let e = |id: &str, hero: bool, order: i32| Evidence {
            id: id.into(),
            spec: Some(EvidenceSpec {
                hero,
                order,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut v = [
            e("a", false, 2),
            e("b", false, 1),
            e("c", true, 9),
            e("d", false, 1),
        ];
        v.sort_by_key(gallery_key);
        let ids: Vec<&str> = v.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "d", "a"]);
    }
}
