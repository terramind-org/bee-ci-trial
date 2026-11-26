//! The tonic `BriefService` over [`Briefs`], following the forge service shape
//! (`docs/spec/forge-core.md` §7).
//!
//! | RPCs | who |
//! |---|---|
//! | `GetBriefChecklist`, `GetEvidence`, `ListEvidence` | repo `read` (signed-out callers on public repos too) |
//! | `SetChangeBrief`, `AddEvidence`, `BeginEvidenceUpload`, `FinalizeEvidence`, `UpdateEvidence` | whoever may edit the change request: its author (with `read`) or `commit` on the repo; CI evidence also needs the caller to be the check run's reporter |
//! | `DeleteEvidence` | as above for uploaded evidence; CI evidence only its reporter or a repo maintainer |
//!
//! Download URLs are signed for the caller's principal chain (empty for a signed-out caller)
//! and expire after 15 minutes; `stale` is computed against the CR's current revision.

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::brief_service_server::{BriefService, BriefServiceServer};
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::MAX_LIST_CHANGES;
use super::model::EvidenceRec;
use super::store::{Briefs, Files, NewEvidence};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::media::ChainLink;
use crate::service::{ForgeServices, next, page, reply};

type Res<T> = std::result::Result<Response<T>, Status>;

/// `BriefService` over a forge and its briefs module.
#[derive(Clone)]
pub struct BriefServices {
    base: ForgeServices,
    briefs: Arc<Briefs>,
}

fn chain_of(caller: &Caller) -> Vec<ChainLink> {
    if caller.is_anonymous() {
        return Vec::new();
    }
    caller
        .token
        .chain()
        .into_iter()
        .map(|p| (p.id, p.kind.as_str().to_string()))
        .collect()
}

fn current_revision(cr: &ChangeRequest) -> u32 {
    cr.revisions.iter().map(|r| r.n).max().unwrap_or(0)
}

impl BriefServices {
    /// The service over `forge` and `briefs`.
    pub fn new(forge: Arc<Forge>, briefs: Arc<Briefs>) -> BriefServices {
        BriefServices {
            base: ForgeServices::new(forge),
            briefs,
        }
    }

    /// The tonic server.
    pub fn brief_service(&self) -> BriefServiceServer<BriefServices> {
        BriefServiceServer::new(self.clone())
    }

    /// Adds `BriefService` to a tonic router builder.
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.brief_service());
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

    /// The right to edit a change request: its author (reading the repo) or `commit` on the
    /// repo; never on an archived repo, and the token must allow `commit`.
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
        let cr = self.briefs.changes().get(&read, repo, change_id).await?;
        if cr
            .author
            .as_ref()
            .is_some_and(|a| a.id == caller.cx.actor_id)
        {
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

    /// The right to delete an item: CI evidence only by its reporter (who may edit the CR) or
    /// a repo maintainer; uploaded evidence by whoever may edit the CR.
    async fn delete_cx(&self, caller: &Caller, repo: &Repo, rec: &EvidenceRec) -> Result<Cx> {
        let e = rec.evidence();
        let ci = e.source != EvidenceSource::Uploaded as i32;
        let reporter = e
            .uploader
            .as_ref()
            .is_some_and(|u| u.id == caller.cx.actor_id);
        if ci && !reporter {
            let maintain = self
                .forge()
                .authz
                .repo(
                    caller,
                    repo,
                    Action::Commit,
                    &[],
                    None,
                    Some(RepoRole::Maintain),
                )
                .await;
            return self
                .base
                .guarded(caller, &repo.org_id, &repo.id, "evidence.delete", maintain)
                .await;
        }
        self.edit_cx(caller, repo, &e.change_request_id, "evidence.delete")
            .await
    }

    /// An item as the caller sees it.
    async fn serve(
        &self,
        caller: &Caller,
        cx: &Cx,
        repo: &Repo,
        rec: &EvidenceRec,
    ) -> Result<Evidence> {
        let cr = self
            .briefs
            .changes()
            .get(cx, repo, &rec.evidence().change_request_id)
            .await?;
        Ok(self.briefs.served(
            rec,
            &chain_of(caller),
            self.forge().db.now_ms(),
            current_revision(&cr),
        ))
    }
}

#[tonic::async_trait]
impl BriefService for BriefServices {
    async fn set_change_brief(
        &self,
        req: Request<SetChangeBriefRequest>,
    ) -> Res<SetChangeBriefResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .edit_cx(&caller, &repo, &r.change_id, "cr.set_brief")
                .await?;
            let brief = r.brief.unwrap_or_default();
            let (cr, checklist) = self
                .briefs
                .set_brief(&cx, &repo, &r.change_id, &brief, r.expected_version)
                .await?;
            Ok(SetChangeBriefResponse {
                brief: cr.brief.map(|b| *b),
                checklist: Some(checklist),
                version: cr.version,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_brief_checklist(
        &self,
        req: Request<GetBriefChecklistRequest>,
    ) -> Res<BriefChecklist> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            self.briefs
                .checklist(&cx, &repo, &r.change_id, r.revision)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn add_evidence(&self, req: Request<AddEvidenceRequest>) -> Res<Evidence> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .edit_cx(&caller, &repo, &r.change_id, "evidence.add")
                .await?;
            let files = match (r.workflow_artifact_id.is_empty(), r.assets.is_empty()) {
                (false, false) => {
                    return Err(ForgeError::invalid(
                        "bad_evidence",
                        "send files inline or from a workflow artifact, not both",
                    ));
                }
                (false, true) => Files::Artifact {
                    id: r.workflow_artifact_id,
                    assets: r.artifact_assets,
                },
                (true, _) if !r.artifact_assets.is_empty() => {
                    return Err(ForgeError::invalid(
                        "bad_evidence",
                        "artifact_assets need workflow_artifact_id",
                    ));
                }
                (true, false) => Files::Inline(r.assets),
                (true, true) => Files::None,
            };
            let spec = r.spec.ok_or_else(|| ForgeError::missing("spec"))?;
            let rec = self
                .briefs
                .add(
                    &cx,
                    &repo,
                    &r.change_id,
                    NewEvidence {
                        spec,
                        revision: r.revision,
                        check_run_id: r.check_run_id,
                        files,
                    },
                )
                .await?;
            self.serve(&caller, &cx, &repo, &rec).await
        }
        .await;
        reply(&caller, out)
    }

    async fn begin_evidence_upload(
        &self,
        req: Request<BeginEvidenceUploadRequest>,
    ) -> Res<BeginEvidenceUploadResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .edit_cx(&caller, &repo, &r.change_id, "evidence.begin_upload")
                .await?;
            let spec = r.spec.ok_or_else(|| ForgeError::missing("spec"))?;
            let rec = self
                .briefs
                .begin(
                    &cx,
                    &repo,
                    &r.change_id,
                    spec,
                    r.revision,
                    &r.check_run_id,
                    &r.assets,
                )
                .await?;
            let now = self.forge().db.now_ms();
            let e = rec.evidence();
            let uploads = e
                .assets
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let (url, exp) =
                        self.briefs
                            .upload_url(&e.repo_id, &e.id, i, now, rec.expires_ms);
                    EvidenceUploadTarget {
                        role: a.role,
                        upload_url: url,
                        upload_headers: Default::default(),
                        expires_at: Some(ts(exp)),
                    }
                })
                .collect();
            Ok(BeginEvidenceUploadResponse {
                evidence: Some(self.serve(&caller, &cx, &repo, &rec).await?),
                uploads,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn finalize_evidence(&self, req: Request<FinalizeEvidenceRequest>) -> Res<Evidence> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let read = self.read_cx(&caller, &repo).await?;
            let rec = self.briefs.get(&read, &repo, &r.evidence_id).await?;
            let cx = self
                .edit_cx(
                    &caller,
                    &repo,
                    &rec.evidence().change_request_id,
                    "evidence.finalize",
                )
                .await?;
            let rec = self
                .briefs
                .finalize(&cx, &repo, &r.evidence_id, &r.digests)
                .await?;
            self.serve(&caller, &cx, &repo, &rec).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_evidence(&self, req: Request<GetEvidenceRequest>) -> Res<Evidence> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let rec = self.briefs.get(&cx, &repo, &r.id).await?;
            self.serve(&caller, &cx, &repo, &rec).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_evidence(&self, req: Request<ListEvidenceRequest>) -> Res<ListEvidenceResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            if r.change_ids.is_empty() {
                return Err(ForgeError::missing("change_ids"));
            }
            if r.change_ids.len() > MAX_LIST_CHANGES {
                return Err(ForgeError::too_large(
                    "change_ids",
                    MAX_LIST_CHANGES as u64,
                    r.change_ids.len() as u64,
                ));
            }
            let (token, size) = page(&r.page);
            let start = if token.is_empty() {
                0
            } else {
                token
                    .parse::<usize>()
                    .map_err(|_| ForgeError::invalid("bad_page_token", "bad page token"))?
            };
            let all = self
                .briefs
                .list(&cx, &repo, &r.change_ids, r.hero_only)
                .await?;
            let mut revisions: BTreeMap<String, u32> = BTreeMap::new();
            let chain = chain_of(&caller);
            let now = self.forge().db.now_ms();
            let mut evidence = Vec::new();
            for rec in all.iter().skip(start).take(size) {
                let cr_id = rec.evidence().change_request_id.clone();
                let current = match revisions.get(&cr_id) {
                    Some(n) => *n,
                    None => {
                        let cr = self.briefs.changes().get(&cx, &repo, &cr_id).await?;
                        let n = current_revision(&cr);
                        revisions.insert(cr_id, n);
                        n
                    }
                };
                evidence.push(self.briefs.served(rec, &chain, now, current));
            }
            let end = start + evidence.len();
            let token = if end < all.len() {
                end.to_string()
            } else {
                String::new()
            };
            Ok(ListEvidenceResponse {
                evidence,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_evidence(&self, req: Request<UpdateEvidenceRequest>) -> Res<Evidence> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let read = self.read_cx(&caller, &repo).await?;
            let rec = self.briefs.get(&read, &repo, &r.id).await?;
            let cx = self
                .edit_cx(
                    &caller,
                    &repo,
                    &rec.evidence().change_request_id,
                    "evidence.update",
                )
                .await?;
            let spec = r.spec.ok_or_else(|| ForgeError::missing("spec"))?;
            let rec = self
                .briefs
                .update(&cx, &repo, &r.id, spec, &r.alt_texts)
                .await?;
            self.serve(&caller, &cx, &repo, &rec).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_evidence(
        &self,
        req: Request<DeleteEvidenceRequest>,
    ) -> Res<DeleteEvidenceResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let read = self.read_cx(&caller, &repo).await?;
            let rec = self.briefs.get(&read, &repo, &r.id).await?;
            let cx = self.delete_cx(&caller, &repo, &rec).await?;
            self.briefs.delete(&cx, &repo, &r.id).await?;
            Ok(DeleteEvidenceResponse {})
        }
        .await;
        reply(&caller, out)
    }
}
