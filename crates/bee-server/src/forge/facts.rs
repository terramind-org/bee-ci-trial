//! Land facts and signature verification for the forge (SPEC §9.4, PRODUCT §4.9).
//!
//! - [`ServerSignatures`] serves `SecurityService.VerifyObjectSignature`: changesets through
//!   [`crate::signatures`], tags through the annotation's armored signature (an annotation
//!   made through the API signs its message bytes).
//! - [`ServerLandFacts`] gives the changes module what it cannot compute itself when it
//!   decides a land: the stack's signature states, the enforced org-required workflows
//!   (F49-007), the open code-scanning alerts on the code being landed (F49-008) and whether
//!   an active merge-queue ruleset matches the target ref. The
//!   coalescer gets the same facts through [`crate::state::ForgeInputs`].

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use bee_core::ObjectId;
use bee_forge::changes::{LandFacts, QueueParams};
use bee_forge::security::ports::SignatureVerifier;
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::{self, Repo, SecuritySignatureReason as R};

use super::ForgeWiring;
use super::adapters::repo_ctx;
use crate::state::AppState;

type FResult<T> = bee_forge::Result<T>;

fn core_id(id: &v1::ObjectId) -> FResult<ObjectId> {
    ObjectId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

fn upgrade(state: &Weak<AppState>) -> FResult<Arc<AppState>> {
    state
        .upgrade()
        .ok_or_else(|| ForgeError::unavailable("server stopping"))
}

/// `VerifyObjectSignature` over the server's stores and the forge's key registry.
pub struct ServerSignatures {
    state: Weak<AppState>,
}

impl ServerSignatures {
    /// A verifier over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerSignatures {
        ServerSignatures {
            state: Arc::downgrade(state),
        }
    }
}

fn scheme_of(v: &bee_forge::community::keys::SignatureVerdict) -> i32 {
    v.scheme as i32
}

#[async_trait]
impl SignatureVerifier for ServerSignatures {
    async fn verify(
        &self,
        repo: &Repo,
        changeset: Option<&v1::ObjectId>,
        tag: &str,
    ) -> FResult<v1::VerifyObjectSignatureResponse> {
        let state = upgrade(&self.state)?;
        let ctx = repo_ctx(&state, &repo.id).await?;
        if let Some(c) = changeset {
            let id = core_id(c)?;
            let sig = crate::signatures::verify_changeset(&state, &ctx, &id)
                .await
                .map_err(super::api_to_forge)?;
            let history = state.history(&ctx);
            let provenance = match history.changeset(&id).await {
                Ok(cs) => history
                    .meta_object(cs.meta())
                    .await
                    .ok()
                    .flatten()
                    .and_then(|m| crate::convert::meta_to(&m).provenance),
                Err(_) => None,
            };
            let signer = match &sig.verdict.signer {
                Some(p) => state
                    .principal(p)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| super::adapters::principal_of(&r))
                    .or_else(|| {
                        Some(v1::Principal {
                            id: p.clone(),
                            ..Default::default()
                        })
                    }),
                None => None,
            };
            return Ok(v1::VerifyObjectSignatureResponse {
                verified: sig.verdict.verified(),
                reason: sig.verdict.reason as i32,
                scheme: scheme_of(&sig.verdict),
                signer_key_id: sig.verdict.key_id.clone(),
                signer,
                signature: sig.verdict.signature.clone(),
                provenance,
                provenance_chain_verified: false,
            });
        }
        let name = tag.trim_start_matches("refs/").trim_start_matches("tags/");
        let annotation = super::adapters::tag_annotation(&state, &ctx, name)
            .await
            .map_err(super::api_to_forge)?
            .ok_or_else(|| ForgeError::not_found(format!("annotated tag {name}")))?;
        if annotation.signature.is_empty() {
            return Ok(v1::VerifyObjectSignatureResponse {
                reason: R::Unsigned as i32,
                ..Default::default()
            });
        }
        let verdict = state
            .forge
            .verify_signature(annotation.message.as_bytes(), &annotation.signature)
            .await
            .map_err(super::api_to_forge)?
            .ok_or_else(|| ForgeError::unavailable("no signing-key registry"))?;
        Ok(v1::VerifyObjectSignatureResponse {
            verified: verdict.verified(),
            reason: verdict.reason as i32,
            scheme: scheme_of(&verdict),
            signer_key_id: verdict.key_id.clone(),
            signer: verdict.signer.as_ref().map(|p| v1::Principal {
                id: p.clone(),
                ..Default::default()
            }),
            signature: verdict.signature.clone(),
            provenance: None,
            provenance_chain_verified: false,
        })
    }
}

/// Land facts for the changes module.
pub struct ServerLandFacts {
    state: Weak<AppState>,
}

impl ServerLandFacts {
    /// Facts over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerLandFacts {
        ServerLandFacts {
            state: Arc::downgrade(state),
        }
    }

    fn wiring(&self) -> FResult<(Arc<AppState>, Arc<ForgeWiring>)> {
        let state = upgrade(&self.state)?;
        let w = state
            .hub
            .get()
            .cloned()
            .ok_or_else(|| ForgeError::unavailable("forge not open yet"))?;
        Ok((state, w))
    }
}

fn hexes(ids: &[v1::ObjectId]) -> Vec<String> {
    ids.iter().map(|i| bee_core::hex::encode(&i.raw)).collect()
}

#[async_trait]
impl LandFacts for ServerLandFacts {
    async fn signatures(
        &self,
        repo: &Repo,
        stack: &[v1::ObjectId],
    ) -> FResult<Vec<bee_policy::SignatureState>> {
        let state = upgrade(&self.state)?;
        let ctx = repo_ctx(&state, &repo.id).await?;
        let ids = stack.iter().map(core_id).collect::<FResult<Vec<_>>>()?;
        crate::signatures::states(&state, &ctx, &ids)
            .await
            .map_err(super::api_to_forge)
    }

    async fn required_workflows(
        &self,
        repo: &Repo,
        heads: &[v1::ObjectId],
    ) -> FResult<Vec<bee_policy::WorkflowGate>> {
        let (_, w) = self.wiring()?;
        w.ci.required_workflow_gates(&Cx::system(), repo, &hexes(heads))
            .await
    }

    async fn code_alerts(
        &self,
        repo: &Repo,
        target_ref: &str,
        heads: &[v1::ObjectId],
    ) -> FResult<Vec<bee_policy::CodeAlert>> {
        let (_, w) = self.wiring()?;
        let raw: Vec<Vec<u8>> = heads.iter().map(|h| h.raw.clone()).collect();
        w.security
            .open_alerts_for_land(&Cx::system(), repo, &raw, &code_scan_refs(target_ref))
            .await
    }

    async fn merge_queue_required(&self, repo: &Repo, target_ref: &str) -> FResult<bool> {
        let state = upgrade(&self.state)?;
        let ctx = repo_ctx(&state, &repo.id).await?;
        crate::services::policy::merge_queue_required(&state, &ctx, target_ref)
            .await
            .map_err(super::api_to_forge)
    }

    async fn merge_queue_params(
        &self,
        repo: &Repo,
        target_ref: &str,
    ) -> FResult<Option<QueueParams>> {
        let state = upgrade(&self.state)?;
        let ctx = repo_ctx(&state, &repo.id).await?;
        crate::services::policy::merge_queue_params(&state, &ctx, target_ref)
            .await
            .map_err(super::api_to_forge)
    }
}

/// The ref names a target ref's code-scanning analyses are uploaded under.
pub(crate) fn code_scan_refs(target_ref: &str) -> Vec<String> {
    let short = target_ref
        .trim_start_matches("refs/heads/")
        .trim_start_matches("refs/");
    vec![short.to_string(), format!("refs/heads/{short}")]
}

impl ForgeWiring {
    /// Enforced required workflows for landing `head` (and the CR head) in a repo.
    pub(crate) async fn workflow_gates(
        &self,
        cx: &Cx,
        repo: &Repo,
        heads: &[String],
    ) -> Vec<bee_policy::WorkflowGate> {
        self.ci
            .required_workflow_gates(cx, repo, heads)
            .await
            .unwrap_or_default()
    }

    /// Open code-scanning alerts for landing `heads` on `ref_name`.
    pub(crate) async fn land_code_alerts(
        &self,
        cx: &Cx,
        repo: &Repo,
        ref_name: &str,
        heads: &[Vec<u8>],
    ) -> crate::errors::ApiResult<Vec<bee_policy::CodeAlert>> {
        self.security
            .open_alerts_for_land(cx, repo, heads, &code_scan_refs(ref_name))
            .await
            .map_err(super::forge_to_api)
    }
}
