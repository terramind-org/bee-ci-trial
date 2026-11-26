//! tonic implementations of `OrgService`, `RepoService`, `LeaseService`, `CheckService`,
//! `EventService`, `AuditService`, `IssueService` and `ProjectService` over a [`Forge`].
//!
//! Every method follows the same shape (see `docs/spec/forge-core.md`):
//!
//! 1. authenticate ([`ForgeServices::caller`]); failures are `UNAUTHENTICATED`,
//!    `TOKEN_EXPIRED` or `TOKEN_REVOKED`;
//! 2. validate the request (`INVALID_ARGUMENT` with a subcode);
//! 3. resolve the subject (repo, org) and authorize ([`crate::auth::Authz`]); denied
//!    state-changing calls are audited;
//! 4. call the module's store method with the authorized [`crate::Cx`];
//! 5. map every error to a `tonic::Status` carrying a SPEC §12.2 `ErrorDetail` with the
//!    request id.
//!
//! RPCs whose work belongs to another subsystem answer `UNIMPLEMENTED` with an
//! `ErrorDetail` naming the owner ([`unserved`]); the list is in `docs/spec/forge-core.md`.

mod audit;
mod checks;
mod comments;
pub(crate) mod events;
mod issues;
mod leases;
mod orgs;
mod projects;
mod repos;
mod work;

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::audit_service_server::AuditServiceServer;
use bee_proto::v1::check_service_server::CheckServiceServer;
use bee_proto::v1::discussion_service_server::DiscussionServiceServer;
use bee_proto::v1::event_service_server::EventServiceServer;
use bee_proto::v1::gist_service_server::GistServiceServer;
use bee_proto::v1::insights_service_server::InsightsServiceServer;
use bee_proto::v1::issue_service_server::IssueServiceServer;
use bee_proto::v1::lease_service_server::LeaseServiceServer;
use bee_proto::v1::notification_service_server::NotificationServiceServer;
use bee_proto::v1::org_service_server::OrgServiceServer;
use bee_proto::v1::package_service_server::PackageServiceServer;
use bee_proto::v1::pages_service_server::PagesServiceServer;
use bee_proto::v1::project_service_server::ProjectServiceServer;
use bee_proto::v1::release_service_server::ReleaseServiceServer;
use bee_proto::v1::repo_service_server::RepoServiceServer;
use bee_proto::v1::sponsor_service_server::SponsorServiceServer;
use bee_proto::v1::user_service_server::UserServiceServer;
use bee_proto::v1::wiki_service_server::WikiServiceServer;
use bee_proto::v1::{PageRequest, PageResponse, Repo, RepoRef};
use tonic::Status;

use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;

/// Default page size.
pub const DEFAULT_PAGE: usize = 50;
/// Largest page size.
pub const MAX_PAGE: usize = 500;

/// A server-streaming response.
pub type ResponseStream<T> =
    Pin<Box<dyn tokio_stream::Stream<Item = std::result::Result<T, Status>> + Send>>;

/// The forge services over one [`Forge`].
#[derive(Clone)]
pub struct ForgeServices {
    forge: Arc<Forge>,
    repo_settings_observer: Option<Arc<dyn RepoSettingsObserver>>,
    /// The access decisions open subscriptions share (see `events::StreamAccess`).
    stream_access: Arc<events::StreamAccess>,
}

/// Runs after a repository settings update commits and before its RPC succeeds.
///
/// Server integrations use this hook to synchronously persist settings needed by their own
/// failure-domain fallbacks. An observer error is returned to the caller even though the forge
/// transaction has already committed.
#[async_trait]
pub trait RepoSettingsObserver: Send + Sync {
    /// Observe committed repository settings.
    async fn updated(&self, settings: &bee_proto::v1::RepoSettings) -> Result<()>;
}

impl ForgeServices {
    /// Services over `forge`.
    pub fn new(forge: Arc<Forge>) -> ForgeServices {
        ForgeServices {
            forge,
            repo_settings_observer: None,
            stream_access: Arc::default(),
        }
    }

    /// How many access decisions open event subscriptions have made so far. Subscriptions
    /// opened with one credential on one log make one between them each second, so this
    /// grows with the credentials in use, not with the streams open.
    pub fn stream_access_decisions(&self) -> u64 {
        self.stream_access.decisions()
    }

    /// Installs an observer that runs after repository settings commit and before success.
    pub fn with_repo_settings_observer(
        mut self,
        observer: Arc<dyn RepoSettingsObserver>,
    ) -> ForgeServices {
        self.repo_settings_observer = Some(observer);
        self
    }

    /// The forge.
    pub fn forge(&self) -> &Arc<Forge> {
        &self.forge
    }

    /// The `OrgService` server.
    pub fn org_service(&self) -> OrgServiceServer<ForgeServices> {
        OrgServiceServer::new(self.clone())
    }

    /// The `RepoService` server.
    pub fn repo_service(&self) -> RepoServiceServer<ForgeServices> {
        RepoServiceServer::new(self.clone())
    }

    /// The `LeaseService` server.
    pub fn lease_service(&self) -> LeaseServiceServer<ForgeServices> {
        LeaseServiceServer::new(self.clone())
    }

    /// The `CheckService` server.
    pub fn check_service(&self) -> CheckServiceServer<ForgeServices> {
        CheckServiceServer::new(self.clone())
    }

    /// The `EventService` server.
    pub fn event_service(&self) -> EventServiceServer<ForgeServices> {
        EventServiceServer::new(self.clone())
    }

    /// The `AuditService` server.
    pub fn audit_service(&self) -> AuditServiceServer<ForgeServices> {
        AuditServiceServer::new(self.clone())
    }

    /// The community services (`DiscussionService`, `WikiService`, `GistService`,
    /// `ReleaseService`, `NotificationService`, `UserService`, `InsightsService`,
    /// `SponsorService`; see `docs/spec/forge-community.md`).
    #[allow(clippy::type_complexity)]
    pub fn community_services(
        &self,
    ) -> (
        DiscussionServiceServer<ForgeServices>,
        WikiServiceServer<ForgeServices>,
        GistServiceServer<ForgeServices>,
        ReleaseServiceServer<ForgeServices>,
        NotificationServiceServer<ForgeServices>,
        UserServiceServer<ForgeServices>,
        InsightsServiceServer<ForgeServices>,
        SponsorServiceServer<ForgeServices>,
    ) {
        (
            DiscussionServiceServer::new(self.clone()),
            WikiServiceServer::new(self.clone()),
            GistServiceServer::new(self.clone()),
            ReleaseServiceServer::new(self.clone()),
            NotificationServiceServer::new(self.clone()),
            UserServiceServer::new(self.clone()),
            InsightsServiceServer::new(self.clone()),
            SponsorServiceServer::new(self.clone()),
        )
    }

    /// The `PackageService` server (see `docs/spec/packages-pages.md`).
    pub fn package_service(&self) -> PackageServiceServer<ForgeServices> {
        PackageServiceServer::new(self.clone())
    }

    /// The `PagesService` server (see `docs/spec/packages-pages.md`).
    pub fn pages_service(&self) -> PagesServiceServer<ForgeServices> {
        PagesServiceServer::new(self.clone())
    }

    /// The `IssueService` server.
    pub fn issue_service(&self) -> IssueServiceServer<ForgeServices> {
        IssueServiceServer::new(self.clone())
    }

    /// The `ProjectService` server.
    pub fn project_service(&self) -> ProjectServiceServer<ForgeServices> {
        ProjectServiceServer::new(self.clone())
    }

    /// Adds every service to a tonic router builder (the server's registration point).
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.org_service());
        routes.add_service(self.repo_service());
        routes.add_service(self.lease_service());
        routes.add_service(self.check_service());
        routes.add_service(self.event_service());
        routes.add_service(self.audit_service());
        let (discussions, wiki, gists, releases, notifications, users, insights, sponsors) =
            self.community_services();
        routes.add_service(discussions);
        routes.add_service(wiki);
        routes.add_service(gists);
        routes.add_service(releases);
        routes.add_service(notifications);
        routes.add_service(users);
        routes.add_service(insights);
        routes.add_service(sponsors);
        routes.add_service(self.issue_service());
        routes.add_service(self.project_service());
        routes.add_service(self.package_service());
        routes.add_service(self.pages_service());
    }

    /// Authenticates a request.
    pub(crate) fn caller<T>(&self, req: &tonic::Request<T>) -> std::result::Result<Caller, Status> {
        self.forge.authn.authenticate(req).map_err(|e| {
            let rid = req
                .metadata()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            e.to_status(rid)
        })
    }

    /// Re-verifies an open stream's bearer credential before a new authorization decision.
    pub(crate) fn fresh(&self, caller: &Caller) -> Result<Caller> {
        self.forge.authn.fresh(caller)
    }

    /// Resolves a repo reference to a live repo.
    pub(crate) async fn repo(&self, caller: &Caller, r: &Option<RepoRef>) -> Result<Repo> {
        let r = r
            .as_ref()
            .filter(|r| r.repo.is_some())
            .ok_or_else(|| ForgeError::missing("repo"))?;
        self.forge.orgs.resolve_repo(&caller.cx, r).await
    }

    /// Resolves an org name or id to a live org id.
    pub(crate) async fn org_id(&self, caller: &Caller, name_or_id: &str) -> Result<String> {
        Ok(self.forge.orgs.get_org(&caller.cx, name_or_id).await?.id)
    }

    /// Runs `f` after authorization `auth`; a denial of a state-changing call is audited as
    /// `action`.
    pub(crate) async fn guarded(
        &self,
        caller: &Caller,
        org_id: &str,
        repo_id: &str,
        action: &str,
        auth: Result<Cx>,
    ) -> Result<Cx> {
        match auth {
            Ok(cx) => Ok(cx),
            Err(e) => {
                self.forge
                    .authz
                    .audit_denial(caller, org_id, repo_id, action, &e)
                    .await;
                Err(e)
            }
        }
    }
}

/// Converts a result to a tonic response.
pub(crate) fn reply<T>(
    caller: &Caller,
    r: Result<T>,
) -> std::result::Result<tonic::Response<T>, Status> {
    r.map(tonic::Response::new)
        .map_err(|e| e.to_status(&caller.cx.request_id))
}

/// Page size and token of a request.
pub(crate) fn page(p: &Option<PageRequest>) -> (String, usize) {
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
pub(crate) fn next(token: String) -> Option<PageResponse> {
    Some(PageResponse {
        next_page_token: token,
    })
}

/// The `UNIMPLEMENTED` answer of an RPC served by another subsystem.
pub fn unserved(rpc: &str, owner: &str) -> ForgeError {
    ForgeError::unimplemented(format!(
        "{rpc} is not served by the forge core; owner: {owner}"
    ))
    .with_subcode("served_elsewhere")
}

/// Decodes raw paths into UTF-8 strings for token and policy checks.
pub(crate) fn utf8_paths(paths: &[Vec<u8>]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|p| crate::leases::normalize_path(p))
        .collect()
}
