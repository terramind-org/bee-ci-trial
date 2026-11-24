//! Shared plumbing of the operator-facing services (search, policy, ops, git, backups, audit
//! sinks, exports): JSON records in the server store, paging, and the operator check.

use bee_policy::Action;
use bee_proto::v1;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::kv::Kv;
use crate::state::AppState;

/// Default page size.
pub const DEFAULT_PAGE: usize = 50;
/// Largest page size.
pub const MAX_PAGE: usize = 500;

/// Page token and size of a request.
pub fn page_of(p: &Option<v1::PageRequest>) -> (String, usize) {
    match p {
        Some(p) => (
            p.page_token.clone(),
            if p.page_size == 0 {
                DEFAULT_PAGE
            } else {
                (p.page_size as usize).min(MAX_PAGE)
            },
        ),
        None => (String::new(), DEFAULT_PAGE),
    }
}

/// A page response.
pub fn next_page(token: String) -> Option<v1::PageResponse> {
    Some(v1::PageResponse {
        next_page_token: token,
    })
}

/// Pages an ordered list by offset tokens: `(items, next token)`.
pub fn offset_page<T: Clone>(all: &[T], token: &str, size: usize) -> ApiResult<(Vec<T>, String)> {
    let start: usize = if token.is_empty() {
        0
    } else {
        token
            .parse()
            .map_err(|_| ApiError::invalid("bad_page_token", "malformed page token"))?
    };
    let end = start.saturating_add(size).min(all.len());
    let items = all.get(start.min(all.len())..end).unwrap_or(&[]).to_vec();
    let next = if end < all.len() {
        end.to_string()
    } else {
        String::new()
    };
    Ok((items, next))
}

/// Reads one JSON record.
pub async fn get_json<T: DeserializeOwned>(
    kv: &dyn Kv,
    ns: &str,
    key: &str,
) -> ApiResult<Option<T>> {
    match kv.get(ns, key.as_bytes()).await? {
        Some(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| ApiError::new(v1::ErrorReason::Internal, format!("bad {ns} row: {e}"))),
        None => Ok(None),
    }
}

/// Writes one JSON record.
pub async fn put_json<T: Serialize>(kv: &dyn Kv, ns: &str, key: &str, v: &T) -> ApiResult<()> {
    let bytes = serde_json::to_vec(v)
        .map_err(|e| ApiError::new(v1::ErrorReason::Internal, format!("encode {ns}: {e}")))?;
    kv.put(ns, key.as_bytes(), bytes).await?;
    Ok(())
}

/// Every JSON record of `ns` under `prefix`, in key order.
pub async fn scan_json<T: DeserializeOwned>(
    kv: &dyn Kv,
    ns: &str,
    prefix: &str,
) -> ApiResult<Vec<(String, T)>> {
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let rows = kv
            .scan(ns, prefix.as_bytes(), after.as_deref(), 1000)
            .await?;
        let done = rows.len() < 1000;
        for (k, v) in rows {
            if let Ok(t) = serde_json::from_slice::<T>(&v) {
                out.push((String::from_utf8_lossy(&k).into_owned(), t));
            }
            after = Some(k);
        }
        if done {
            return Ok(out);
        }
    }
}

/// Whether `id` is listed in `ops.operators` or `forge.site_admins`.
fn listed_operator(state: &AppState, id: &str) -> bool {
    state.cfg.ops.operators.iter().any(|o| o == id)
        || state.cfg.forge.site_admins.iter().any(|o| o == id)
}

/// Whether every principal of the caller's chain is a server operator (listed in
/// `ops.operators` or `forge.site_admins`). Administering an org never makes a principal an
/// operator: operator RPCs act on every tenant of the server.
pub fn is_operator(state: &AppState, caller: &Caller) -> bool {
    let chain = caller.chain_ids();
    !chain.is_empty() && chain.iter().all(|id| listed_operator(state, id))
}

/// Server-wide operator RPCs (logs, metrics, traces, config, rollouts, status-page writes,
/// backups and restores, signing keys; `docs/spec/operations.md` §1.1): every principal of
/// the chain is a configured operator or site admin, and the token allows `manage_settings`.
pub async fn require_operator(state: &AppState, caller: &Caller) -> ApiResult<()> {
    if !caller.token.allows_action(Action::ManageSettings) {
        return Err(ApiError::policy(
            v1::ErrorReason::PolicyDenied,
            "token",
            vec!["token:manage_settings".into()],
            "operator calls need a token that allows manage_settings",
        ));
    }
    if is_operator(state, caller) {
        return Ok(());
    }
    Err(ApiError::policy(
        v1::ErrorReason::PolicyDenied,
        "operators",
        vec!["role:operator".into()],
        format!("{} is not an operator of this server", caller.id()),
    ))
}

/// Whether `principal` administers the org `org_id` (VCS org id text): a server-store org
/// admin row for that org, or an owner or admin of the org in the forge.
async fn administers(state: &AppState, org_id: &str, principal: &str) -> ApiResult<bool> {
    let key = format!("{org_id}/{principal}");
    if state
        .kv
        .get(crate::auth::NS_ORG_ADMIN, key.as_bytes())
        .await?
        .is_some()
    {
        return Ok(true);
    }
    Ok(state
        .forge
        .org_admins(org_id)
        .await
        .iter()
        .any(|a| a == principal))
}

/// Whether every principal of the caller's chain administers the org `org_id` (VCS org id
/// text). Only that org counts: administering another org grants nothing here, and
/// delegation does not widen it (an agent needs its own admin role in the org).
pub async fn is_org_admin(state: &AppState, caller: &Caller, org_id: &str) -> ApiResult<bool> {
    let chain = caller.chain_ids();
    if chain.is_empty() {
        return Ok(false);
    }
    for id in &chain {
        if !administers(state, org_id, id).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The org a request names, by name or id (`NOT_FOUND` when unknown).
pub async fn org_by_name_or_id(state: &AppState, org: &str) -> ApiResult<bee_store::OrgRow> {
    if org.is_empty() {
        return Err(ApiError::invalid("missing_field", "org is required"));
    }
    if let Some(o) = state.meta.org_by_name(org).await? {
        return Ok(o);
    }
    if let Ok(u) = bee_core::Ulid::parse(org)
        && let Some(o) = state.meta.get_org(bee_store::OrgId(u)).await?
    {
        return Ok(o);
    }
    Err(ApiError::not_found(format!("org {org} not found")))
}

/// Every org of the metadata store (read straight from its `orgs` table; the store trait
/// has no listing).
pub async fn all_orgs(state: &AppState) -> ApiResult<Vec<bee_store::OrgRow>> {
    let m = &state.cfg.metadata;
    let ids = if m.backend == "postgres" {
        let schema = if m.postgres_schema.is_empty() {
            "public".to_string()
        } else {
            m.postgres_schema.clone()
        };
        crate::backup::sql::pg_org_ids(&m.postgres_url, &schema).await?
    } else {
        crate::backup::sql::sqlite_org_ids(state.cfg.data_path(&m.sqlite_path, "meta.db")).await?
    };
    let mut out = Vec::new();
    for id in ids {
        if let Some(o) = state.meta.get_org(bee_store::OrgId(id)).await? {
            out.push(o);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Every repo of every org, ordered by full name.
pub async fn all_repos(state: &AppState) -> ApiResult<Vec<crate::state::RepoCtx>> {
    let mut out = Vec::new();
    for org in all_orgs(state).await? {
        for r in state.meta.list_repos(org.id).await? {
            out.push(crate::state::RepoCtx {
                full_name: format!("{}/{}", org.name, r.name),
                org: org.clone(),
                repo: r,
            });
        }
    }
    out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    Ok(out)
}

/// Runs a unary handler: authenticates the caller, then maps the result with the request id.
#[macro_export]
#[doc(hidden)]
macro_rules! unary {
    ($state:expr, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {{
        let $meta = $crate::state::RequestMeta::of(&$req);
        let r = async {
            let $caller = $state.caller($req.metadata()).await?;
            let $inner = $req.into_inner();
            $body.await.map(tonic::Response::new)
        }
        .await;
        $crate::services::object::finish(&$meta, r)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_pages_cover_everything_once() {
        let all: Vec<u32> = (0..7).collect();
        let (a, t) = offset_page(&all, "", 3).unwrap();
        assert_eq!(a, vec![0, 1, 2]);
        let (b, t) = offset_page(&all, &t, 3).unwrap();
        assert_eq!(b, vec![3, 4, 5]);
        let (c, t) = offset_page(&all, &t, 3).unwrap();
        assert_eq!(c, vec![6]);
        assert!(t.is_empty());
        assert!(offset_page(&all, "x", 3).is_err());
    }
}
