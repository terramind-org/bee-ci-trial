//! The forge's half of the repo-access decision (`crate::access`): repo records, effective
//! roles and name resolution, each bounded by a short deadline (a timeout reads as
//! `UNAVAILABLE`, so the decision fails closed), and the event listener that drops cached
//! decisions when the forge relays a change to access.

use std::sync::Weak;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bee_forge::ForgeError;
use bee_proto::v1::{self, ErrorReason, RepoRole};

use super::{ForgeWiring, forge_to_api, landing};
use crate::errors::{ApiError, ApiResult};
use crate::state::{AppState, RepoCtx};

fn deadlines() -> (Instant, Duration) {
    let now = Instant::now();
    crate::state::REQUEST_FORGE_DEADLINES
        .try_with(|(request, lookup)| (*request, *lookup))
        .unwrap_or((now + Duration::from_secs(30), Duration::from_secs(3)))
}

async fn bounded<T, F, Fut>(request_deadline: Instant, lookup: Duration, mut f: F) -> ApiResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ApiResult<T>>,
{
    let attempt = tokio::time::timeout(
        lookup.min(request_deadline.saturating_duration_since(Instant::now())),
        f(),
    )
    .await;
    match attempt {
        Ok(r) => r,
        Err(_) if request_deadline.saturating_duration_since(Instant::now()) >= lookup => {
            match tokio::time::timeout(
                lookup.min(request_deadline.saturating_duration_since(Instant::now())),
                f(),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(ApiError::new(
                    ErrorReason::Unavailable,
                    "forge lookup timed out",
                )),
            }
        }
        Err(_) => Err(ApiError::new(
            ErrorReason::Unavailable,
            "forge lookup timed out",
        )),
    }
}

fn absent(e: &ForgeError) -> bool {
    matches!(
        e.reason,
        ErrorReason::NotFound | ErrorReason::InvalidArgument
    )
}

impl ForgeWiring {
    /// The forge record of `repo` for access decisions: by id (forks and transferred repos
    /// keep their id), else the parent repo of a wiki repo (`<name>.wiki`); a repo of a
    /// soft-deleted org reads as deleted. `None` when the forge keeps no record.
    pub async fn access_record(&self, repo: &RepoCtx) -> ApiResult<Option<v1::Repo>> {
        let (request_deadline, lookup) = deadlines();
        bounded(request_deadline, lookup, || async {
            let cx = landing::quick_cx();
            let mut rec = match self.forge.orgs.repo_any(&cx, &repo.id_text()).await {
                Ok(r) => Some(r),
                Err(e) if absent(&e) => None,
                Err(e) => return Err(forge_to_api(e)),
            };
            if rec.is_none()
                && let Some(base) = repo.repo.name.strip_suffix(".wiki")
            {
                let r = v1::RepoRef {
                    repo: Some(v1::repo_ref::Repo::FullName(format!(
                        "{}/{base}",
                        repo.org.name
                    ))),
                };
                rec = match self.forge.orgs.resolve_repo(&cx, &r).await {
                    Ok(r) => Some(r),
                    Err(e) if absent(&e) => None,
                    Err(e) => return Err(forge_to_api(e)),
                };
            }
            if let Some(r) = rec.as_mut()
                && r.deleted_at.is_none()
            {
                match self.forge.orgs.org_any(&cx, &r.org_id).await {
                    Ok(o) if o.deleted_at.is_some() => r.deleted_at = o.deleted_at,
                    Ok(_) => {}
                    Err(e) if absent(&e) => {}
                    Err(e) => return Err(forge_to_api(e)),
                }
            }
            Ok(rec)
        })
        .await
    }

    /// `principal_id`'s effective role on the forge repo `repo`.
    pub async fn access_role(&self, repo: &v1::Repo, principal_id: &str) -> ApiResult<RepoRole> {
        let (request_deadline, lookup) = deadlines();
        bounded(request_deadline, lookup, || async {
            let cx = landing::quick_cx();
            let perm = self
                .forge
                .orgs
                .effective_role(&cx, repo, principal_id)
                .await
                .map_err(forge_to_api)?;
            Ok(RepoRole::try_from(perm.role).unwrap_or(RepoRole::None))
        })
        .await
    }

    /// The id of the live forge repo called `full_name` (following rename and transfer
    /// redirects), if any.
    pub async fn repo_by_full_name(&self, full_name: &str) -> ApiResult<Option<String>> {
        let (request_deadline, lookup) = deadlines();
        bounded(request_deadline, lookup, || async {
            let cx = landing::quick_cx();
            let r = v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(full_name.to_string())),
            };
            match self.forge.orgs.resolve_repo(&cx, &r).await {
                Ok(r) => Ok(Some(r.id)),
                Err(e) if absent(&e) => Ok(None),
                Err(e) => Err(forge_to_api(e)),
            }
        })
        .await
    }
}

/// Whether a relayed forge event can change who may access a repo.
pub fn changes_access(kind: &str) -> bool {
    const PREFIXES: [&str; 12] = [
        "repo.",
        "deploy_key.",
        "org.",
        "member.",
        "collaborator.",
        "repo_invitation.",
        "team.",
        "app.",
        "role.",
        "enterprise.",
        "sponsor",
        "scim.",
    ];
    PREFIXES.iter().any(|p| kind.starts_with(p))
}

/// Drops cached repo-access decisions when the forge relays a change to repo records,
/// memberships, teams, collaborators, installations or sponsorships (changes made through
/// another server process that shares this forge database are bounded by
/// `forge.access_cache_ms` instead).
pub struct AccessInvalidator {
    /// The server state.
    pub state: Weak<AppState>,
}

#[async_trait]
impl bee_forge::events::EventListener for AccessInvalidator {
    async fn on_event(&self, event: &v1::Event) {
        if !changes_access(&event.kind) {
            return;
        }
        if let Some(state) = self.state.upgrade() {
            state.forget_access();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn access_changing_events_are_recognised() {
        for k in [
            "repo.archived",
            "repo.transferred",
            "repo.deleted",
            "member.removed",
            "collaborator.added",
            "team.repo_added",
            "app.uninstalled",
            "org.deleted",
        ] {
            assert!(changes_access(k), "{k}");
        }
        for k in ["issue.opened", "cr.landed", "push", "check.updated"] {
            assert!(!changes_access(k), "{k}");
        }
    }

    #[tokio::test]
    async fn bounded_lookup_retries_one_timeout_inside_the_request_deadline() {
        let calls = AtomicUsize::new(0);
        let answer = bounded(
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(10),
            || async {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Ok::<_, ApiError>("answer")
            },
        )
        .await
        .unwrap();
        assert_eq!(answer, "answer");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bounded_lookup_returns_unavailable_after_two_timeouts() {
        let calls = AtomicUsize::new(0);
        let error = bounded(
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(10),
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                Ok::<_, ApiError>("answer")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.detail.reason, "UNAVAILABLE");
        assert_eq!(error.detail.message, "forge lookup timed out");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bounded_lookup_does_not_retry_a_non_timeout_error() {
        let calls = AtomicUsize::new(0);
        let error = bounded(
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(10),
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(ApiError::invalid("bad_lookup", "invalid lookup"))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.detail.message, "invalid lookup");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
