//! Request-level enterprise hooks for forge services, run by the request layer before the
//! handler: authentication against the server's token path (so deactivated principals are
//! refused here as on every VCS RPC), enterprise admission (IP allow lists, deprovisioned
//! principals, SSO and 2FA) for the orgs the request touches, moderation of writes the forge
//! handlers do not guard themselves, and an event-bridge sync before event reads.
//!
//! The orgs a request touches come from its message: the top-level `repo` (`RepoRef`) and
//! `org` fields, read by transcoding the request with the descriptor table
//! (`bee_proto::methods`). VCS services are not inspected here: they call admission from
//! `AppState::gate` with the repo they resolved.

use std::sync::Arc;

use bee_forge::enterprise::moderation::WriteKind;
use bee_proto::v1;
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;

use super::ForgeWiring;
use crate::errors::{ApiError, ApiResult};
use crate::state::AppState;

/// Services whose handlers resolve their repo and call admission themselves.
pub const CORE_SERVICES: [&str; 7] = [
    "bee.v1.ObjectService",
    "bee.v1.QueryService",
    "bee.v1.WriteService",
    "bee.v1.RefService",
    "bee.v1.AuthService",
    "bee.v1.AdminService",
    "bee.v1.ImportService",
];

/// Services whose writes change admission state (blocks, allow lists, SSO, SCIM tokens,
/// membership); the admission cache is cleared after their calls.
pub const ADMISSION_SERVICES: [&str; 5] = [
    "bee.v1.EnterpriseService",
    "bee.v1.TeamService",
    "bee.v1.ModerationService",
    "bee.v1.OrgService",
    "bee.v1.AppService",
];

/// Services whose writes can change who may access a repo (repo records and lifecycle,
/// memberships, teams and roles, collaborators, installations, sponsorships); cached
/// repo-access decisions are dropped after their writes (`crate::access`).
pub const ACCESS_SERVICES: [&str; 6] = [
    "bee.v1.RepoService",
    "bee.v1.OrgService",
    "bee.v1.TeamService",
    "bee.v1.EnterpriseService",
    "bee.v1.AppService",
    "bee.v1.SponsorService",
];

/// Whether a call can change repo access: a method of an [`ACCESS_SERVICES`] service that
/// is not a read (`Get…`, `List…`, `Search…`, `Watch…`, `Check…`).
pub fn changes_access(path: &str) -> bool {
    let method = path.rsplit('/').next().unwrap_or_default();
    ACCESS_SERVICES.contains(&service_of(path))
        && !["Get", "List", "Search", "Watch", "Check"]
            .iter()
            .any(|p| method.starts_with(p))
}

/// The service name of a gRPC path.
pub fn service_of(path: &str) -> &str {
    path.trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default()
}

/// Whether the layer inspects requests of this path.
pub fn inspects(path: &str) -> bool {
    !CORE_SERVICES.contains(&service_of(path))
}

fn moderated(path: &str) -> Option<WriteKind> {
    match path {
        "/bee.v1.ChangeService/SubmitReview" => Some(WriteKind::Review),
        "/bee.v1.CommentService/AddReaction"
        | "/bee.v1.DiscussionService/AddReaction"
        | "/bee.v1.ChangeService/AddChangeReaction" => Some(WriteKind::Reaction),
        "/bee.v1.RepoService/ForkRepo" => Some(WriteKind::Fork),
        _ => None,
    }
}

/// Whether the RPC takes one request message (unary or server streaming), so the layer may
/// buffer its body.
pub fn single_request(path: &str) -> bool {
    use bee_proto::methods::MethodKind;
    bee_proto::methods::find_method(path)
        .is_some_and(|m| matches!(m.kind, MethodKind::Unary | MethodKind::ServerStreaming))
}

/// The JSON form of a unary request's message (the body is one gRPC frame).
fn message_json(path: &str, body: &[u8]) -> Option<Value> {
    let m = bee_proto::methods::find_method(path)?;
    if !single_request(path) || body.len() < 5 || body[0] != 0 {
        return None;
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    let msg = body.get(5..5 + len)?;
    let json = (m.request_proto_to_json)(msg).ok()?;
    serde_json::from_slice(&json).ok()
}

fn repo_ref_of(v: &Value) -> Option<v1::RepoRef> {
    let r = v.get("repo")?;
    if let Some(id) = r.get("id").and_then(Value::as_str) {
        return Some(v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(id.to_string())),
        });
    }
    r.get("fullName")
        .and_then(Value::as_str)
        .map(|n| v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::FullName(n.to_string())),
        })
}

/// Runs the hooks for one forge request; `body` is the buffered request for unary calls.
pub async fn check(
    state: &Arc<AppState>,
    w: &Arc<ForgeWiring>,
    headers: &HeaderMap,
    path: &str,
    body: Option<&Bytes>,
) -> ApiResult<()> {
    let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(());
    };
    if token.contains('.') {
        // An app JWT (only `AppService` accepts one; the forge verifies it).
        return Ok(());
    }
    let (verified, _) = state.verify_token(token).await?;
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let msg = body.and_then(|b| message_json(path, b));
    let mut repo: Option<crate::state::RepoCtx> = None;
    let mut orgs: Vec<String> = Vec::new();
    if let Some(v) = &msg {
        if let Some(r) = repo_ref_of(v)
            && let Ok(ctx) = state.repo(Some(&r)).await
        {
            orgs.push(ctx.org.id.0.to_text());
            repo = Some(ctx);
        }
        if let Some(o) = v
            .get("org")
            .and_then(Value::as_str)
            .filter(|o| !o.is_empty())
        {
            let cx = super::landing::quick_cx();
            if let Ok(org) = w.forge.orgs.get_org(&cx, o).await {
                orgs.push(org.id);
            }
        }
    }
    let principals: Vec<String> = verified.chain().into_iter().map(|p| p.id).collect();
    let app = w.app_token(&verified).await;
    w.admit(&principals, &orgs, &ip, app).await?;
    if let (Some(kind), Some(ctx)) = (moderated(path), &repo) {
        w.moderate(&verified, &ctx.id_text(), &ctx.org.id.0.to_text(), kind)
            .await?;
    }
    if service_of(path) == "bee.v1.EventService"
        && let Some(ctx) = &repo
    {
        w.sync_repo(&ctx.id_text(), &super::ForgeWiring::bridge_org(ctx))
            .await;
    }
    Ok(())
}

/// The admission error answer for the layer.
pub fn refuse(e: ApiError, request_id: &str) -> http::Response<tonic::body::Body> {
    e.with_request_id(request_id)
        .into_status()
        .into_http::<tonic::body::Body>()
}
