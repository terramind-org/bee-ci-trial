//! `ImportService` for git history (SPEC §17): import a local git repository or an uploaded
//! packfile into a repo with `bee-import`, track progress, and serve the fidelity report.
//! GitHub imports run `bee-github` here with the forge as their sink (`crate::forge::import`);
//! exports and other forges answer `UNIMPLEMENTED`.

use std::path::Path;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use bee_core::{ObjectId, Ulid};
use bee_derived::Registry;
use bee_history::{EagerRows, History};
use bee_import::{DerivedHook, ImportOptions, ImportReport, ImportTarget, RefPublisher};
use bee_object::Changeset;
use bee_policy::{Action, Policy, PrincipalKind};
use bee_proto::v1::{self, ErrorReason, import_service_server::ImportService};
use bee_store::{DerivedRow, OrgRow, RefUpdate, RefUpdated, RepoId, RepoRow};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::kv::Kv;
use crate::services::Rpc;
use crate::services::object::finish;
use crate::state::{AppState, RepoCtx, RequestMeta};
use crate::util::timestamp_ms;

/// Namespace of import records.
pub const NS_IMPORT: &str = "import";

/// Checks safety at each ref CAS for an import rather than holding a permit through the
/// object walk, remote fetch, and derived-data writes.
pub(crate) struct ImportPublisher {
    state: Weak<AppState>,
    repo: RepoCtx,
}

impl ImportPublisher {
    pub(crate) fn new(state: &Arc<AppState>, repo: RepoCtx) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::downgrade(state),
            repo,
        })
    }

    async fn publish_ref(&self, update: RefUpdate) -> ApiResult<RefUpdated> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(crate::forge::safety_unavailable)?;
        let fence = state
            .hub
            .fence_ref_publication(&state, &self.repo, true)
            .await?;
        crate::forge::publish_ref(&state, update, fence).await
    }
}

#[async_trait]
impl RefPublisher for ImportPublisher {
    async fn publish(&self, update: RefUpdate) -> bee_core::Result<RefUpdated> {
        self.publish_ref(update).await.map_err(|e| {
            let detail = *e.detail;
            let reason =
                bee_core::Reason::parse(&detail.reason).unwrap_or(bee_core::Reason::Internal);
            bee_core::BeeError::new(reason, detail.message)
                .with_payload(bee_core::error::SUBCODE_KEY, detail.subcode)
        })
    }
}

/// Eager derived rows for imported changesets (graph, path history and any registered
/// eager type), the same rows CreateChangeset writes.
pub struct EagerHook {
    registry: Arc<Registry>,
}

impl EagerHook {
    /// A hook writing `registry`'s eager rows.
    pub fn new(registry: Arc<Registry>) -> EagerHook {
        EagerHook { registry }
    }
}

#[async_trait]
impl DerivedHook for EagerHook {
    async fn rows(
        &self,
        target: &ImportTarget,
        changeset: &Changeset,
    ) -> bee_core::Result<Vec<DerivedRow>> {
        let history = History::new(target.writer.clone(), target.org, target.repo);
        self.registry
            .rows(&history, changeset.id(), changeset)
            .await
    }
}

/// A stored import.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportRecord {
    /// Id.
    pub id: String,
    /// Source label.
    pub source: String,
    /// Target repo ids.
    pub repo_ids: Vec<String>,
    /// Phase (proto value).
    pub phase: i32,
    /// State (proto value).
    pub state: i32,
    /// Progress per category: (name, done, total).
    pub progress: Vec<(String, u64, u64)>,
    /// Start time.
    pub started_ms: u64,
    /// When it finished.
    pub finished_ms: u64,
    /// Principal that started it.
    pub principal: String,
    /// Report JSON, when finished.
    pub report: String,
    /// Failure message.
    pub error: String,
}

async fn load(kv: &dyn Kv, id: &str) -> ApiResult<ImportRecord> {
    kv.get(NS_IMPORT, id.as_bytes())
        .await?
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| ApiError::not_found(format!("import {id} not found")))
}

async fn save(kv: &dyn Kv, r: &ImportRecord) -> ApiResult<()> {
    Ok(kv
        .put(
            NS_IMPORT,
            r.id.as_bytes(),
            serde_json::to_vec(r).unwrap_or_default(),
        )
        .await?)
}

fn to_proto(r: &ImportRecord) -> v1::Import {
    v1::Import {
        id: r.id.clone(),
        source: r.source.clone(),
        repo_ids: r.repo_ids.clone(),
        phase: r.phase,
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
        eta: None,
        rate_limit_note: String::new(),
        started_at: Some(timestamp_ms(r.started_ms)),
        live_since: (r.state == v1::ImportState::Live as i32).then(|| timestamp_ms(r.finished_ms)),
    }
}

/// The import service.
pub struct Imports {
    state: Arc<AppState>,
}

impl Imports {
    async fn provisioning_fence(
        &self,
        caller: &Caller,
        org: &OrgRow,
    ) -> ApiResult<Option<bee_forge::safety::control::LandFence>> {
        if caller.token.principal.kind == PrincipalKind::Human || !self.state.cfg.forge.enabled {
            return Ok(None);
        }
        let wiring = self
            .state
            .hub
            .get()
            .ok_or_else(crate::forge::safety_unavailable)?;
        bee_forge::safety::control::fence_landing(
            &wiring.forge.db,
            &crate::forge::landing::quick_cx(),
            &org.id.0.to_text(),
        )
        .await
        .map(Some)
        .map_err(crate::forge::forge_to_api)
    }

    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> Imports {
        Imports { state }
    }
}

enum Source {
    Path(std::path::PathBuf),
    Pack { pack: ObjectId, refs: ObjectId },
}

fn parse_source(url: &str) -> ApiResult<Source> {
    if let Some(rest) = url.strip_prefix("pack:") {
        let (pack, refs) = rest.split_once("?refs=").ok_or_else(|| {
            ApiError::invalid(
                "invalid_source",
                "pack sources are `pack:<blob id>?refs=<blob id>`",
            )
        })?;
        return Ok(Source::Pack {
            pack: ObjectId::parse(pack).map_err(ApiError::from)?,
            refs: ObjectId::parse(refs).map_err(ApiError::from)?,
        });
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    if path.is_empty() {
        return Err(ApiError::invalid("missing_field", "source_url is required"));
    }
    Ok(Source::Path(std::path::PathBuf::from(path)))
}

impl Imports {
    async fn org_admin(&self, caller: &Caller, org: &OrgRow) -> ApiResult<()> {
        let text = self.state.org_text(org).await?;
        let policy = Policy::from_sources(Some(&text), None).map_err(|e| {
            ApiError::new(ErrorReason::Internal, format!("invalid org policy: {e}"))
        })?;
        for a in &caller.actors {
            if !policy.actions_of(a).contains(&Action::ManageSettings) {
                return Err(ApiError::policy(
                    ErrorReason::PolicyDenied,
                    "members",
                    vec!["role:manage_settings".into()],
                    "importing needs manage_settings in the target org",
                ));
            }
        }
        Ok(())
    }

    /// The target repo, created (with its empty root) if it does not exist. Importing into
    /// an existing repo rewrites its refs, so it passes the repo gate for `manage_settings`
    /// (the forge record's lifecycle and the caller's role, token scope, policy): a deleted
    /// repo is `NOT_FOUND`, an archived one `ARCHIVED`.
    async fn target_repo(&self, caller: &Caller, org: &OrgRow, name: &str) -> ApiResult<RepoRow> {
        let s = &self.state;
        if let Some(r) = s.meta.repo_by_name(org.id, name).await? {
            let ctx = s
                .repo(Some(&v1::RepoRef {
                    repo: Some(v1::repo_ref::Repo::Id(r.id.0.to_text())),
                }))
                .await?;
            s.gate(caller, &ctx, Action::ManageSettings, None).await?;
            return Ok(r);
        }
        let repo = RepoRow {
            id: RepoId(crate::util::new_ulid(s.now_ms())?),
            org: org.id,
            name: name.to_string(),
            created_at_ms: s.now_ms(),
            draft_retention_days: bee_store::DEFAULT_DRAFT_RETENTION_DAYS,
        };
        crate::server::create_repo(s, org, &repo, false).await?;
        Ok(repo)
    }

    async fn start_impl(
        &self,
        caller: &Caller,
        req: v1::StartImportRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::Import> {
        let s = self.state.clone();
        match v1::ImportSourceKind::try_from(req.source_kind)
            .unwrap_or(v1::ImportSourceKind::Unspecified)
        {
            v1::ImportSourceKind::Git => {}
            v1::ImportSourceKind::Unspecified
                if !req.source_url.is_empty()
                    && req.github_installation_id.is_empty()
                    && req.github_token.is_empty() => {}
            v1::ImportSourceKind::Github | v1::ImportSourceKind::Unspecified => {
                return self.start_github(caller, req, meta).await;
            }
            _ => {
                return Err(ApiError::unimplemented(
                    "GitLab, Bitbucket, Azure DevOps, Gitea and archive imports are not served; this server imports GitHub repositories and git history",
                ));
            }
        }
        if req.org {
            return Err(ApiError::invalid(
                "unsupported_scope",
                "git imports target one repository",
            ));
        }
        let (org_name, repo_name) = match req.source.split_once('/') {
            Some((o, r)) => (
                if req.target_org.is_empty() {
                    o.to_string()
                } else {
                    req.target_org.clone()
                },
                r.to_string(),
            ),
            None if !req.target_org.is_empty() => (req.target_org.clone(), req.source.clone()),
            None => {
                return Err(ApiError::invalid(
                    "invalid_source",
                    "source must be `org/repo` (or set target_org)",
                ));
            }
        };
        let org = s
            .meta
            .org_by_name(&org_name)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("org {org_name} not found")))?;
        self.org_admin(caller, &org).await?;
        if req.phase == v1::ImportPhase::DryRun as i32 {
            return Err(ApiError::invalid(
                "unsupported_phase",
                "git imports have no dry run; import into a scratch repo",
            ));
        }
        let source = parse_source(&req.source_url)?;
        if let Source::Path(p) = &source {
            let roots = &s.cfg.import.allowed_roots;
            let canon = std::fs::canonicalize(p)
                .map_err(|_| ApiError::not_found("the source path does not exist"))?;
            if !roots.iter().any(|r| canon.starts_with(Path::new(r))) {
                return Err(ApiError::denied(
                    "local-path imports are limited to import.allowed_roots",
                ));
            }
        }
        let fence = self.provisioning_fence(caller, &org).await?;
        let repo = self.target_repo(caller, &org, &repo_name).await?;
        drop(fence);
        let id = Ulid::to_text(&crate::util::new_ulid(s.now_ms())?);
        let record = ImportRecord {
            id: id.clone(),
            source: format!("git:{}", req.source_url),
            repo_ids: vec![repo.id.0.to_text()],
            phase: if req.phase == 0 {
                v1::ImportPhase::Mirror as i32
            } else {
                req.phase
            },
            state: v1::ImportState::Queued as i32,
            progress: Vec::new(),
            started_ms: s.now_ms(),
            finished_ms: 0,
            principal: caller.id().to_string(),
            report: String::new(),
            error: String::new(),
        };
        save(s.kv.as_ref(), &record).await?;
        s.audit(
            caller,
            meta,
            None,
            &id,
            "import",
            json!({ "repo": repo.id.0.to_text() }),
        )
        .await?;
        let dry_run = record.phase == v1::ImportPhase::DryRun as i32;
        let creator = importer_principal(&s, &org, caller.id()).await?;
        let kv = s.kv.clone();
        let st = s.clone();
        let publisher = (caller.token.principal.kind != PrincipalKind::Human).then(|| {
            ImportPublisher::new(
                &st,
                RepoCtx {
                    org: org.clone(),
                    repo: repo.clone(),
                    full_name: format!("{}/{}", org.name, repo.name),
                },
            ) as Arc<dyn RefPublisher>
        });
        tokio::spawn(async move {
            let mut rec = record;
            rec.state = v1::ImportState::Git as i32;
            let _ = save(kv.as_ref(), &rec).await;
            let result = run(&st, &org, &repo, source, &creator, dry_run, publisher).await;
            rec.finished_ms = st.now_ms();
            match result {
                Ok(report) => {
                    rec.progress = report
                        .categories
                        .iter()
                        .map(|c| (c.category.clone(), c.imported, c.imported + c.dropped))
                        .collect();
                    rec.report = report.to_json();
                    rec.state = v1::ImportState::Live as i32;
                    let _ = st
                        .events
                        .append(
                            repo.id,
                            "import.finished",
                            &json!({ "import": rec.id, "summary": report.summary }),
                            st.now_ms(),
                        )
                        .await;
                }
                Err(e) => {
                    rec.state = v1::ImportState::Failed as i32;
                    rec.error = e.detail.message;
                }
            }
            let _ = save(kv.as_ref(), &rec).await;
        });
        Ok(to_proto(&load(self.state.kv.as_ref(), &id).await?))
    }
}

impl Imports {
    /// `StartImport` of a GitHub repository: the `bee-github` importer with the server's
    /// stores as the git target and the forge as the metadata sink, in the background.
    async fn start_github(
        &self,
        caller: &Caller,
        req: v1::StartImportRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::Import> {
        let s = self.state.clone();
        let Some(w) = s.hub.get().cloned() else {
            if s.cfg.forge.enabled && caller.token.principal.kind != PrincipalKind::Human {
                return Err(crate::forge::safety_unavailable());
            }
            return Err(ApiError::unimplemented(
                "GitHub imports need the forge, which is not running on this server",
            ));
        };
        if req.org {
            return Err(ApiError::invalid(
                "unsupported_scope",
                "GitHub imports here target one repository; import an org repo by repo",
            ));
        }
        let Some((owner, name)) = req.source.split_once('/') else {
            return Err(ApiError::invalid(
                "invalid_source",
                "source must be the GitHub repository `owner/name`",
            ));
        };
        let org_name = if req.target_org.is_empty() {
            owner.to_string()
        } else {
            req.target_org.clone()
        };
        let org = s
            .meta
            .org_by_name(&org_name)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("org {org_name} not found")))?;
        self.org_admin(caller, &org).await?;
        let credential =
            crate::forge::import::credential(&s, &req.github_token, &req.github_installation_id)?;
        let fence = self.provisioning_fence(caller, &org).await?;
        let repo = self.target_repo(caller, &org, name).await?;
        drop(fence);
        let id = Ulid::to_text(&crate::util::new_ulid(s.now_ms())?);
        let entry = w
            .github_importer(
                &id,
                &req.source,
                &org,
                &repo,
                caller.id(),
                &importer_principal(&s, &org, caller.id()).await?,
                credential,
            )
            .await?;
        let dry_run = req.phase == v1::ImportPhase::DryRun as i32;
        let mut record = ImportRecord {
            id: id.clone(),
            source: format!("github:{}", req.source),
            repo_ids: vec![repo.id.0.to_text()],
            phase: if req.phase == 0 {
                v1::ImportPhase::Mirror as i32
            } else {
                req.phase
            },
            state: v1::ImportState::Queued as i32,
            progress: Vec::new(),
            started_ms: s.now_ms(),
            finished_ms: 0,
            principal: caller.id().to_string(),
            report: String::new(),
            error: String::new(),
        };
        if dry_run {
            let est = entry.importer.dry_run().await?;
            record.progress = vec![
                ("pull_requests".into(), 0, est.pull_requests),
                ("issues".into(), 0, est.issues),
                ("requests".into(), 0, est.requests),
            ];
            record.state = v1::ImportState::Live as i32;
            record.finished_ms = s.now_ms();
        }
        save(s.kv.as_ref(), &record).await?;
        s.audit(
            caller,
            meta,
            None,
            &id,
            "import",
            json!({ "repo": repo.id.0.to_text(), "github": req.source }),
        )
        .await?;
        if !dry_run {
            let kv = s.kv.clone();
            let st = s.clone();
            tokio::spawn(async move {
                let mut rec = record;
                rec.state = v1::ImportState::Git as i32;
                let _ = save(kv.as_ref(), &rec).await;
                let result = entry.importer.run().await;
                rec.finished_ms = st.now_ms();
                match result {
                    Ok(r) => {
                        rec.progress = r
                            .report
                            .git
                            .categories
                            .iter()
                            .map(|c| (c.category.clone(), c.imported, c.imported + c.dropped))
                            .collect();
                        rec.report = r.report.to_json();
                        rec.state = v1::ImportState::Live as i32;
                        let _ = st
                            .events
                            .append(
                                repo.id,
                                "import.finished",
                                &json!({ "import": rec.id, "summary": r.report.git.summary }),
                                st.now_ms(),
                            )
                            .await;
                    }
                    Err(e) => {
                        rec.state = v1::ImportState::Failed as i32;
                        rec.error = e.message;
                    }
                }
                let _ = save(kv.as_ref(), &rec).await;
            });
        }
        Ok(to_proto(&load(self.state.kv.as_ref(), &id).await?))
    }
}

/// The git part of a stored report: a git import's report, or a GitHub import's fidelity
/// report (whose GitHub categories are appended to the git ones).
fn stored_report(text: &str) -> Option<(ImportReport, Vec<String>)> {
    if let Ok(f) = bee_github::FidelityReport::from_json(text) {
        return Some((f.git, f.github.manual_steps));
    }
    ImportReport::from_json(text).ok().map(|r| (r, Vec::new()))
}

/// Server KV namespace mapping an org id to its importer service principal.
pub const NS_IMPORTER: &str = "importer_principal";

/// The org's importer service principal (SPEC §9.1: "services (CI, the importer, bots)"):
/// imported changesets, uploads and ref-log entries are attributed to it rather than to the
/// person who started the import, who is recorded on the import and in its audit entry. It is
/// created on the first import into the org, owned by the admin who started it, and holds no
/// token: imports run inside the server after `StartImport` checked that the caller
/// administers the target org (ref administration and write on every path).
pub(crate) async fn importer_principal(
    s: &AppState,
    org: &bee_store::OrgRow,
    starter: &str,
) -> ApiResult<String> {
    let key = org.id.0.to_text();
    loop {
        if let Some(v) = s.kv.get(NS_IMPORTER, key.as_bytes()).await? {
            return Ok(String::from_utf8_lossy(&v).into_owned());
        }
        let id = crate::util::new_ulid(s.now_ms())?.to_text();
        let rec = crate::auth::PrincipalRecord {
            id: id.clone(),
            kind: "service".into(),
            display: format!("{} importer", org.name),
            org: key.clone(),
            owner: starter.to_string(),
            runtime: String::new(),
            purpose: "imports history into the org".into(),
            external_ids: Vec::new(),
            groups: Vec::new(),
            deactivated: false,
        };
        let body = serde_json::to_vec(&rec)
            .map_err(|e| ApiError::new(ErrorReason::Internal, e.to_string()))?;
        let created =
            s.kv.apply(vec![
                crate::kv::KvOp::expect(NS_IMPORTER, key.as_bytes().to_vec(), None),
                crate::kv::KvOp::put(NS_IMPORTER, key.as_bytes().to_vec(), id.as_bytes().to_vec()),
                crate::kv::KvOp::put(crate::auth::NS_PRINCIPAL, id.as_bytes().to_vec(), body),
            ])
            .await?;
        if created {
            return Ok(id);
        }
        // Another import created it first: read theirs.
    }
}

async fn run(
    s: &AppState,
    org: &OrgRow,
    repo: &RepoRow,
    source: Source,
    creator: &str,
    dry_run: bool,
    publisher: Option<Arc<dyn RefPublisher>>,
) -> ApiResult<ImportReport> {
    let target = ImportTarget {
        writer: s.writer.clone(),
        org: org.id,
        repo: repo.id,
    };
    let mut opts = ImportOptions::new(creator);
    opts.at_ms = s.now_ms();
    opts.ref_publisher = publisher;
    opts.derived = Some(Arc::new(EagerHook {
        registry: s.registry.clone(),
    }));
    if dry_run {
        return Err(ApiError::invalid(
            "unsupported_phase",
            "dry runs are not supported for git imports; import into a scratch repo",
        ));
    }
    let out = match source {
        Source::Path(p) => bee_import::import_repository(&p, &target, &opts).await?,
        Source::Pack { pack, refs } => {
            let pack_bytes = s
                .blobs
                .get(org.id, &pack)
                .await?
                .ok_or_else(|| ApiError::not_found("pack blob not found"))?;
            let refs_bytes = bee_store::trees::read_object_bytes(
                s.meta.as_ref(),
                s.blobs.as_ref(),
                org.id,
                &refs,
                bee_store::ReadFrom::Primary,
            )
            .await?;
            let format = if refs_bytes
                .split(|b| *b == b' ' || *b == b'\t')
                .next()
                .is_some_and(|h| h.len() == 64)
            {
                bee_git::ObjectFormat::Sha256
            } else {
                bee_git::ObjectFormat::Sha1
            };
            let list = bee_import::parse_ref_list(&refs_bytes, format)?;
            opts.source = format!("pack:{pack}");
            bee_import::import_pack(pack_bytes, format, &list, None, &target, &opts).await?
        }
    };
    Ok(out.report)
}

macro_rules! import_rpc {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {{
        let $meta = RequestMeta::of(&$req);
        let r = async {
            let $caller = $self.state.caller($req.metadata()).await?;
            let $inner = $req.into_inner();
            $body.await.map(tonic::Response::new)
        }
        .await;
        finish(&$meta, r)
    }};
}

impl Imports {
    async fn visible(&self, caller: &Caller, id: &str) -> ApiResult<ImportRecord> {
        let rec = load(self.state.kv.as_ref(), id).await?;
        if rec.principal != caller.id() {
            // Others may see it when they can read the target repo.
            let repo = self
                .state
                .repo(Some(&v1::RepoRef {
                    repo: Some(v1::repo_ref::Repo::Id(
                        rec.repo_ids.first().cloned().unwrap_or_default(),
                    )),
                }))
                .await
                .map_err(|_| ApiError::not_found(format!("import {id} not found")))?;
            self.state
                .gate(caller, &repo, Action::Read, None)
                .await
                .map_err(|_| ApiError::not_found(format!("import {id} not found")))?;
        }
        Ok(rec)
    }
}

#[tonic::async_trait]
impl ImportService for Imports {
    async fn start_import(&self, req: tonic::Request<v1::StartImportRequest>) -> Rpc<v1::Import> {
        import_rpc!(self, req, |caller, inner, meta| self
            .start_impl(&caller, inner, &meta))
    }

    async fn get_import(&self, req: tonic::Request<v1::GetImportRequest>) -> Rpc<v1::Import> {
        import_rpc!(self, req, |caller, inner, meta| async {
            Ok::<_, ApiError>(to_proto(&self.visible(&caller, &inner.id).await?))
        })
    }

    async fn list_imports(
        &self,
        req: tonic::Request<v1::ListImportsRequest>,
    ) -> Rpc<v1::ListImportsResponse> {
        import_rpc!(self, req, |caller, inner, meta| async {
            let _ = inner;
            let rows = self.state.kv.scan(NS_IMPORT, b"", None, 1000).await?;
            let imports = rows
                .into_iter()
                .filter_map(|(_, v)| serde_json::from_slice::<ImportRecord>(&v).ok())
                .filter(|r| r.principal == caller.id())
                .map(|r| to_proto(&r))
                .collect();
            Ok::<_, ApiError>(v1::ListImportsResponse {
                imports,
                page: Some(v1::PageResponse::default()),
            })
        })
    }

    async fn set_import_phase(
        &self,
        req: tonic::Request<v1::SetImportPhaseRequest>,
    ) -> Rpc<v1::Import> {
        import_rpc!(self, req, |caller, inner, meta| async {
            let mut rec = self.visible(&caller, &inner.id).await?;
            if rec.principal != caller.id() {
                return Err(ApiError::denied(
                    "only the principal that started an import changes its phase",
                ));
            }
            rec.phase = inner.phase;
            save(self.state.kv.as_ref(), &rec).await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &rec.id,
                    "import_phase",
                    json!({ "phase": inner.phase }),
                )
                .await?;
            Ok(to_proto(&rec))
        })
    }

    async fn cancel_import(&self, req: tonic::Request<v1::CancelImportRequest>) -> Rpc<v1::Import> {
        import_rpc!(self, req, |caller, inner, meta| async {
            let mut rec = self.visible(&caller, &inner.id).await?;
            if rec.state == v1::ImportState::Queued as i32
                || rec.state == v1::ImportState::Git as i32
            {
                // Git imports are resumable batches; cancelling stops tracking and keeps what was
                // written (re-running resumes).
                rec.state = v1::ImportState::Cancelled as i32;
                save(self.state.kv.as_ref(), &rec).await?;
                self.state
                    .audit(&caller, &meta, None, &rec.id, "import_cancel", json!({}))
                    .await?;
            }
            Ok::<_, ApiError>(to_proto(&rec))
        })
    }

    async fn get_fidelity_report(
        &self,
        req: tonic::Request<v1::GetFidelityReportRequest>,
    ) -> Rpc<v1::FidelityReport> {
        import_rpc!(self, req, |caller, inner, meta| async {
            let rec = self.visible(&caller, &inner.id).await?;
            if rec.report.is_empty() {
                return Err(ApiError::new(
                    ErrorReason::InvalidState,
                    format!("import {} has no report yet", rec.id),
                ));
            }
            let (report, manual_steps) = stored_report(&rec.report)
                .ok_or_else(|| ApiError::new(ErrorReason::Internal, "stored report unreadable"))?;
            Ok(v1::FidelityReport {
                import_id: rec.id.clone(),
                items: report
                    .categories
                    .iter()
                    .map(|c| v1::FidelityItem {
                        category: c.category.clone(),
                        imported: c.imported,
                        archived: c.archived,
                        dropped: c.dropped,
                        drops: c
                            .drops
                            .iter()
                            .map(|d| v1::FidelityDrop {
                                r#ref: d.item.clone(),
                                reason: d.reason.clone(),
                            })
                            .collect(),
                    })
                    .collect(),
                manual_steps,
                generated_at: Some(timestamp_ms(rec.finished_ms)),
            })
        })
    }

    async fn claim_identity(
        &self,
        req: tonic::Request<v1::ClaimIdentityRequest>,
    ) -> Rpc<v1::ClaimIdentityResponse> {
        import_rpc!(self, req, |caller, inner, meta| async {
            let w = self.state.hub.get().cloned().ok_or_else(|| {
                ApiError::unimplemented("claims need the forge, which is not running here")
            })?;
            let (github_id, login) =
                crate::forge::import::verify_github_code(&self.state, &inner.github_oauth_code)
                    .await?;
            let moved = w.claim_github(github_id, caller.id()).await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &format!("github:{github_id}"),
                    "claim",
                    json!({ "login": login, "moved": moved }),
                )
                .await?;
            Ok::<_, ApiError>(v1::ClaimIdentityResponse {
                principal: Some(v1::Principal {
                    id: caller.id().to_string(),
                    kind: v1::PrincipalKind::Human as i32,
                    display: caller
                        .record
                        .as_ref()
                        .map(|r| r.display.clone())
                        .unwrap_or_default(),
                    external_ids: vec![format!("github:{github_id}")],
                    ..Default::default()
                }),
                placeholders_merged: moved.min(u64::from(u32::MAX)) as u32,
            })
        })
    }

    async fn list_placeholder_principals(
        &self,
        req: tonic::Request<v1::ListPlaceholderPrincipalsRequest>,
    ) -> Rpc<v1::ListPlaceholderPrincipalsResponse> {
        import_rpc!(self, req, |caller, inner, _meta| async {
            let mut recs = Vec::new();
            if inner.import_id.is_empty() {
                let org = self
                    .state
                    .meta
                    .org_by_name(&inner.target_org)
                    .await?
                    .ok_or_else(|| ApiError::not_found("set import_id or target_org"))?;
                self.org_admin(&caller, &org).await?;
                let rows = self.state.kv.scan(NS_IMPORT, b"", None, 10_000).await?;
                for (_, v) in rows {
                    if let Ok(r) = serde_json::from_slice::<ImportRecord>(&v) {
                        recs.push(r);
                    }
                }
                let mut mine = Vec::new();
                for r in recs {
                    let Some(rid) = r.repo_ids.first() else {
                        continue;
                    };
                    if let Ok(ctx) = self
                        .state
                        .repo(Some(&v1::RepoRef {
                            repo: Some(v1::repo_ref::Repo::Id(rid.clone())),
                        }))
                        .await
                        && ctx.org.id == org.id
                    {
                        mine.push(r);
                    }
                }
                recs = mine;
            } else {
                recs.push(self.visible(&caller, &inner.import_id).await?);
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut out = Vec::new();
            for r in recs {
                let Ok(f) = bee_github::FidelityReport::from_json(&r.report) else {
                    continue;
                };
                for p in f.github.placeholders {
                    if p.claimed || !seen.insert(p.github_id) {
                        continue;
                    }
                    out.push(v1::PlaceholderPrincipal {
                        principal: Some(v1::Principal {
                            id: p.principal.clone(),
                            kind: if p.kind == "agent" {
                                v1::PrincipalKind::Agent as i32
                            } else {
                                v1::PrincipalKind::Human as i32
                            },
                            display: p.login.clone(),
                            external_ids: vec![format!("github:{}", p.github_id)],
                            ..Default::default()
                        }),
                        source_login: p.login,
                        source_id: p.github_id.to_string(),
                        contributions: p.contributions,
                    });
                }
            }
            let (token, size) = (
                crate::services::query::page_token(inner.page.as_ref()),
                crate::services::query::page_size(inner.page.as_ref()),
            );
            let start: usize = token.and_then(|t| t.parse().ok()).unwrap_or(0);
            let next = if out.len() > start + size {
                (start + size).to_string()
            } else {
                String::new()
            };
            let page: Vec<_> = out.into_iter().skip(start).take(size).collect();
            Ok::<_, ApiError>(v1::ListPlaceholderPrincipalsResponse {
                placeholders: page,
                page: Some(v1::PageResponse {
                    next_page_token: next,
                }),
            })
        })
    }

    async fn start_export(&self, req: tonic::Request<v1::StartExportRequest>) -> Rpc<v1::Export> {
        import_rpc!(self, req, |caller, inner, meta| {
            crate::services::export::start(&self.state, &caller, inner, &meta)
        })
    }

    async fn get_export(&self, req: tonic::Request<v1::GetExportRequest>) -> Rpc<v1::Export> {
        import_rpc!(self, req, |caller, inner, meta| {
            crate::services::export::get(&self.state, &caller, &inner.id)
        })
    }

    async fn list_exports(
        &self,
        req: tonic::Request<v1::ListExportsRequest>,
    ) -> Rpc<v1::ListExportsResponse> {
        import_rpc!(self, req, |caller, inner, meta| {
            crate::services::export::list(&self.state, &caller, inner)
        })
    }

    async fn cancel_export(&self, req: tonic::Request<v1::CancelExportRequest>) -> Rpc<v1::Export> {
        import_rpc!(self, req, |caller, inner, meta| {
            crate::services::export::cancel(&self.state, &caller, &inner.id, &meta)
        })
    }
}
