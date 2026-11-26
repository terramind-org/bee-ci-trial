//! Authentication and the authorization pattern of every forge RPC.
//!
//! 1. **Authenticate.** [`Authenticator::authenticate`] reads `authorization: Bearer
//!    <biscuit>`, verifies it with `bee_auth` (signature, key id, revocation, expiry) and
//!    builds the call's [`Cx`]: actor and delegation chain from the token, request id
//!    (`x-request-id` or a new ULID), source address (`x-forwarded-for` or the peer),
//!    user agent, and deadline (`grpc-timeout`, else [`crate::cx::DEFAULT_DEADLINE`]).
//! 2. **Authorize** with [`Authz`]:
//!    - repo calls ([`Authz::repo`]): the token's scope must allow the policy action on the
//!      repo, paths and ref ([`authorize_repo_scope`]); every principal of
//!      the chain must hold a repo role whose built-in actions include it
//!      (`bee_policy::builtin_role_actions` over [`crate::orgs::Orgs::effective_role`]);
//!      and when a [`PolicySource`] supplies the repo policy, `check_access_chain` must allow
//!      each path, each principal carrying `@repo-role/<role>` for its own repo role
//!      ([`Authz::policy_actors`]). A principal who cannot even read a private repo gets
//!      `NOT_FOUND`.
//!    - org calls ([`Authz::org`]): every principal of the chain must hold an org role that
//!      grants the [`OrgPerm`]; managing calls also need a token whose `repos` dimension is
//!      unrestricted and that allows `manage_settings`.
//! 3. The returned `Cx` carries the decision (`token:ok role:<role> via <sources> ...`) into
//!    the audit entry. Denied state-changing calls are audited with the reason as `result`
//!    ([`Authz::audit_denial`]).

use std::sync::Arc;
use std::time::Duration;

use bee_auth::{
    AuthError, Request as ScopeRequest, RevocationList, VerifiedToken, Verifier, VerifyingKeys,
};
use bee_policy::{Action, PrincipalKind as PolicyKind, builtin_role_actions};
use bee_proto::v1::{ErrorReason, Principal, PrincipalKind, Repo, RepoRole, Role, Visibility};
use tokio::time::Instant;

use crate::audit::{AuditDraft, AuditLog};
use crate::cx::{Cx, DEFAULT_DEADLINE};
use crate::error::{ForgeError, Result};
use crate::hooks::{PolicySource, PrincipalDirectory};
use crate::ids::{Clock, IdGen};
use crate::orgs::{Orgs, role_name, role_rank};

/// The principal of the server's anonymous read token: a request without credentials to a
/// public read RPC (`forge.allow_anonymous_public_read`) acts as this principal, which holds
/// no role anywhere, so it reads public repos and public profiles only. Not a ULID, so no
/// stored principal has it. Defined here, below the server, so the forge's read RPCs can tell
/// a signed-out caller from a signed-in one.
pub const ANONYMOUS_PRINCIPAL: &str = "anonymous";

/// A verified caller.
#[derive(Clone)]
pub struct Caller {
    /// The verified token.
    pub token: VerifiedToken,
    /// The bearer credential, retained only so open streams can re-verify it.
    credential: String,
    /// The call context (not yet carrying an authorization decision).
    pub cx: Cx,
}

impl Caller {
    /// Constructs a caller for a host that has already verified this bearer credential.
    pub fn from_verified(token: VerifiedToken, credential: impl Into<String>, cx: Cx) -> Caller {
        let cx = cx.with_verified_token(&token);
        Caller {
            token,
            credential: credential.into(),
            cx,
        }
    }

    /// The acting principal.
    pub fn principal(&self) -> Principal {
        self.cx.principal()
    }

    /// True for a request without credentials, carried by the anonymous read token.
    pub fn is_anonymous(&self) -> bool {
        self.cx.actor_id == ANONYMOUS_PRINCIPAL
    }

    /// The bearer credential: open streams of one credential share their access decisions.
    pub(crate) fn credential(&self) -> &str {
        &self.credential
    }
}

fn proto_kind(k: PolicyKind) -> PrincipalKind {
    match k {
        PolicyKind::Human => PrincipalKind::Human,
        PolicyKind::Agent => PrincipalKind::Agent,
        PolicyKind::Service => PrincipalKind::Service,
    }
}

/// Parses a `grpc-timeout` header value (`<digits><unit>`).
pub fn parse_grpc_timeout(v: &str) -> Option<Duration> {
    if v.len() < 2 {
        return None;
    }
    let (n, unit) = v.split_at(v.len() - 1);
    let n: u64 = n.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n.saturating_mul(3600)),
        "M" => Duration::from_secs(n.saturating_mul(60)),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// Verifies bearer tokens.
pub struct Authenticator {
    keys: std::sync::RwLock<VerifyingKeys>,
    revocations: Arc<dyn RevocationList + Send + Sync>,
    clock: Arc<dyn Clock>,
    ids: Arc<IdGen>,
    directory: Arc<dyn PrincipalDirectory>,
}

impl Authenticator {
    /// An authenticator trusting `keys`.
    pub fn new(
        keys: VerifyingKeys,
        revocations: Arc<dyn RevocationList + Send + Sync>,
        ids: Arc<IdGen>,
        directory: Arc<dyn PrincipalDirectory>,
    ) -> Authenticator {
        Authenticator {
            keys: std::sync::RwLock::new(keys),
            revocations,
            clock: ids.clock().clone(),
            ids,
            directory,
        }
    }

    /// Replaces the trusted keys (after the server rotates or retires a signing key, so
    /// tokens signed by a new key verify here too).
    pub fn set_keys(&self, keys: VerifyingKeys) {
        *self.keys.write().unwrap_or_else(|p| p.into_inner()) = keys;
    }

    /// Verifies the request's token and builds its context.
    pub fn authenticate<T>(&self, req: &tonic::Request<T>) -> Result<Caller> {
        let md = req.metadata();
        let get = |k: &str| {
            md.get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let request_id = match get("x-request-id") {
            s if !s.is_empty() && s.len() <= 128 => s,
            _ => self.ids.ulid()?,
        };
        let auth = get("authorization");
        let Some(token) = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
        else {
            return Err(ForgeError::new(
                ErrorReason::Unauthenticated,
                "missing bearer token",
            ));
        };
        let credential = token.trim().to_string();
        let now = self.clock.now_ms() / 1000;
        let token = {
            let keys = self.keys.read().unwrap_or_else(|p| p.into_inner());
            Verifier::new(&keys, self.revocations.as_ref()).verify(&credential, now)?
        };
        let deadline = parse_grpc_timeout(&get("grpc-timeout"))
            .unwrap_or(DEFAULT_DEADLINE)
            .min(Duration::from_secs(300));
        let ip = match get("x-forwarded-for") {
            s if !s.is_empty() => s.split(',').next().unwrap_or("").trim().to_string(),
            _ => req
                .remote_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_default(),
        };
        let kind = proto_kind(token.principal.kind);
        let mut cx = Cx::new(token.principal.id.clone(), kind)
            .with_chain(token.chain().into_iter().map(|p| p.id).collect())
            .with_deadline(Instant::now() + deadline)
            .with_verified_token(&token);
        cx.request_id = request_id;
        cx.ip = ip;
        cx.user_agent = get("user-agent");
        cx.reason = get("x-bee-reason");
        for p in token.chain() {
            self.directory.observe(&Principal {
                id: p.id.clone(),
                kind: proto_kind(p.kind) as i32,
                ..Default::default()
            });
        }
        Ok(Caller {
            token,
            credential,
            cx,
        })
    }

    /// Re-verifies an already authenticated caller's bearer credential for an open stream.
    ///
    /// The stream retains its request context except its deadline, but this repeats signature,
    /// expiry and revocation verification rather than trusting the decision made when it opened.
    /// Each re-decision gets a fresh [`DEFAULT_DEADLINE`] budget: the request deadline bounded
    /// the call that opened the stream, not the stream itself (SPEC §15.3).
    pub fn fresh(&self, caller: &Caller) -> Result<Caller> {
        let now = self.clock.now_ms() / 1000;
        let token = {
            let keys = self.keys.read().unwrap_or_else(|p| p.into_inner());
            Verifier::new(&keys, self.revocations.as_ref()).verify(&caller.credential, now)?
        };
        let mut cx = caller.cx.clone().with_verified_token(&token);
        cx.deadline = Instant::now() + DEFAULT_DEADLINE;
        cx.actor_id = token.principal.id.clone();
        cx.chain = token.chain().into_iter().map(|p| p.id).collect();
        Ok(Caller {
            token,
            credential: caller.credential.clone(),
            cx,
        })
    }
}

/// Org-level permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrgPerm {
    /// See the member list (members only).
    ViewMembers,
    /// Manage profile, settings, members, invitations, webhooks, custom properties.
    Manage,
    /// Rename, delete, restore the org; manage owners.
    Own,
    /// Read the audit log (owners, admins, security managers).
    ReadAudit,
    /// Create repos (owners, admins; members when the org settings allow).
    CreateRepo,
}

/// Checks `token`'s scope against a request on `repo`; `request` builds the request for a
/// repo key (adding paths and ref as the call needs). Every forge check of a token against a
/// repo goes through here, so the forge names repos exactly as core does: both apply
/// [`VerifiedToken::authorize_repo`] (SPEC §9.2).
///
/// The key is the repo's full name (`owner/name`), as core's gate uses and as tokens minted by
/// clients list. Tokens may list repo ids instead (the forge's own minters do: CI job tokens,
/// agent run tokens, app installation and OAuth tokens), so a token that lists this repo's id
/// as an exact entry is also accepted. Patterns are never matched against the id, so a prefix
/// pattern meant for names cannot reach a repo through its id. The refusal returned is the one
/// for the full name.
pub fn authorize_repo_scope(
    token: &VerifiedToken,
    repo: &Repo,
    request: impl Fn(String) -> ScopeRequest,
    now: u64,
) -> std::result::Result<(), AuthError> {
    token.authorize_repo(&repo.full_name, &repo.id, request, now)
}

/// Authorization over forge data, token scope and policy.
pub struct Authz {
    orgs: Arc<Orgs>,
    policy: Option<Arc<dyn PolicySource>>,
    audit: Arc<AuditLog>,
    now: Arc<dyn Clock>,
}

impl Authz {
    /// Authorization reading roles from `orgs`.
    pub fn new(
        orgs: Arc<Orgs>,
        audit: Arc<AuditLog>,
        policy: Option<Arc<dyn PolicySource>>,
        clock: Arc<dyn Clock>,
    ) -> Authz {
        Authz {
            orgs,
            policy,
            audit,
            now: clock,
        }
    }

    fn now_secs(&self) -> u64 {
        self.now.now_ms() / 1000
    }

    /// Authorizes `action` on `repo` (paths and ref when the action is path- or ref-scoped);
    /// `min_role` additionally requires a role rank (e.g. admin for delete and transfer).
    pub async fn repo(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: Action,
        paths: &[String],
        ref_name: Option<&str>,
        min_role: Option<RepoRole>,
    ) -> Result<Cx> {
        authorize_repo_scope(
            &caller.token,
            repo,
            |key| {
                let req = ScopeRequest::new(key, action).paths(paths.iter().cloned());
                match ref_name {
                    Some(r) => req.on_ref(r),
                    None => req,
                }
            },
            self.now_secs(),
        )?;
        let mut decision = vec!["token:ok".to_string()];
        let lookup = caller.cx.chain.clone();
        let mut roles: Vec<(String, RepoRole)> = Vec::with_capacity(lookup.len());
        for id in &lookup {
            let perm = self.orgs.effective_role(&caller.cx, repo, id).await?;
            let role = RepoRole::try_from(perm.role).unwrap_or(RepoRole::None);
            roles.push((id.clone(), role));
            let actions = role_name(role)
                .and_then(builtin_role_actions)
                .unwrap_or_default();
            if !actions.contains(&Action::Read)
                && Visibility::try_from(repo.visibility).unwrap_or(Visibility::Private)
                    != Visibility::Public
            {
                return Err(ForgeError::not_found(format!("repo {}", repo.full_name)));
            }
            let enough = actions.contains(&action)
                && min_role.is_none_or(|m| role_rank(role) >= role_rank(m));
            if !enough {
                return Err(ForgeError::permission_denied(format!(
                    "{id} has role {} on {}, which does not allow `{action}`{}",
                    role_name(role).unwrap_or("none"),
                    repo.full_name,
                    min_role
                        .map(|m| format!(" (needs {})", role_name(m).unwrap_or("?")))
                        .unwrap_or_default()
                )));
            }
            decision.push(format!(
                "{id}:{} via {}",
                role_name(role).unwrap_or("none"),
                perm.sources.join("+")
            ));
        }
        if let Some(src) = &self.policy
            && let Some(policy) = src.policy(repo).await?
        {
            let actors = self.actors_with_roles(caller, repo, &roles).await?;
            let check_paths: Vec<&str> = if paths.is_empty() {
                vec![""]
            } else {
                paths.iter().map(String::as_str).collect()
            };
            for p in check_paths {
                let d = policy.check_access_chain(&actors, action, p);
                if !d.allowed {
                    return Err(ForgeError::policy_denied(
                        &d.rule,
                        vec![format!("{action}:{p}")],
                        d.detail,
                    ));
                }
                decision.push(format!("policy:{}", d.rule));
            }
        }
        let mut cx = caller.cx.clone();
        cx.decision = decision.join(" ");
        Ok(cx)
    }

    /// The token's principal chain as `repo`'s policy sees it (SPEC §9.4): each principal
    /// carries the external group `repo-role/<role>` naming its own effective repo role
    /// ([`Orgs::effective_role`]; no agent inheritance, so policy narrows agents), and none
    /// without a role. Same order and ids as the token's chain.
    pub async fn policy_actors(
        &self,
        caller: &Caller,
        repo: &Repo,
    ) -> Result<Vec<bee_policy::Actor>> {
        self.actors_with_roles(caller, repo, &[]).await
    }

    /// [`Authz::policy_actors`], reusing roles already looked up for this repo by id.
    async fn actors_with_roles(
        &self,
        caller: &Caller,
        repo: &Repo,
        known: &[(String, RepoRole)],
    ) -> Result<Vec<bee_policy::Actor>> {
        let mut out = Vec::new();
        for actor in caller.token.policy_chain(|_| (Vec::new(), Vec::new())) {
            let role = match known.iter().find(|(id, _)| id == actor.id.as_str()) {
                Some((_, r)) => *r,
                None => {
                    let perm = self
                        .orgs
                        .effective_role(&caller.cx, repo, actor.id.as_str())
                        .await?;
                    RepoRole::try_from(perm.role).unwrap_or(RepoRole::None)
                }
            };
            out.push(actor.with_repo_role(role_name(role)));
        }
        Ok(out)
    }

    /// As [`Authz::repo`] for an operation that hands out whole trees (source archives): the
    /// action must hold on the root and on every path the repo's policy has an access rule
    /// for, so a principal kept out of any part of the repo is refused.
    pub async fn repo_every_path(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: Action,
    ) -> Result<Cx> {
        let mut paths = vec![String::new()];
        if let Some(src) = &self.policy
            && let Some(policy) = src.policy(repo).await?
        {
            paths.extend(policy.doc().access.keys().cloned());
        }
        self.repo(caller, repo, action, &paths, None, None).await
    }

    /// Authorizes an org-level permission for every principal of the chain.
    pub async fn org(&self, caller: &Caller, org_id: &str, perm: OrgPerm) -> Result<Cx> {
        if matches!(perm, OrgPerm::Manage | OrgPerm::Own) {
            if caller.token.scope.repos.is_some() {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "org management needs a token not restricted to repos",
                ));
            }
            if !caller.token.allows_action(Action::ManageSettings) {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "token does not allow `manage_settings`",
                ));
            }
        } else if perm == OrgPerm::CreateRepo && !caller.token.allows_action(Action::ManageSettings)
        {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `manage_settings`",
            ));
        } else if !caller.token.allows_action(Action::Read) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `read`",
            ));
        }
        let settings = if perm == OrgPerm::CreateRepo {
            Some(self.orgs.settings(&caller.cx, org_id).await?)
        } else {
            None
        };
        let mut decision = vec!["token:ok".to_string()];
        for id in caller.cx.chain.clone() {
            let member = self.orgs.member(&caller.cx, org_id, &id).await?;
            let role = member
                .as_ref()
                .map(|m| Role::try_from(m.role).unwrap_or(Role::Member));
            let ok = match (perm, role) {
                (_, None) => false,
                (OrgPerm::ViewMembers, Some(_)) => true,
                (OrgPerm::Own, Some(r)) => r == Role::Owner,
                (OrgPerm::Manage, Some(r)) => matches!(r, Role::Owner | Role::Admin),
                (OrgPerm::ReadAudit, Some(r)) => {
                    matches!(r, Role::Owner | Role::Admin | Role::SecurityManager)
                }
                (OrgPerm::CreateRepo, Some(r)) => {
                    matches!(r, Role::Owner | Role::Admin)
                        || settings
                            .as_ref()
                            .is_some_and(|s| s.members_can_create_repos)
                }
            };
            if !ok {
                return Err(ForgeError::permission_denied(format!(
                    "{id} lacks the org permission {perm:?} in {org_id}"
                )));
            }
            decision.push(format!(
                "{id}:org:{}",
                role.map(|r| r.as_str_name()).unwrap_or("none")
            ));
        }
        let mut cx = caller.cx.clone();
        cx.decision = decision.join(" ");
        Ok(cx)
    }

    /// Records a denied state-changing call in the org's audit log (best effort: a failure to
    /// audit never changes the answer the caller gets).
    pub async fn audit_denial(
        &self,
        caller: &Caller,
        org_id: &str,
        repo_id: &str,
        action: &str,
        err: &ForgeError,
    ) {
        if !matches!(
            err.reason,
            ErrorReason::PermissionDenied | ErrorReason::PolicyDenied | ErrorReason::NotFound
        ) || org_id.is_empty()
        {
            return;
        }
        let mut cx = caller.cx.clone();
        cx.decision = format!("denied: {}", err.message);
        let entry = AuditDraft::new(action, org_id)
            .repo_id(repo_id)
            .result(err.reason_name())
            .into_entry(&cx, self.now.now_ms(), String::new());
        let _ = self.audit.append(&cx, entry).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ManualClock;

    #[test]
    fn grpc_timeouts_parse() {
        assert_eq!(parse_grpc_timeout("5S"), Some(Duration::from_secs(5)));
        assert_eq!(parse_grpc_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_grpc_timeout("x"), None);
        assert_eq!(parse_grpc_timeout("10Q"), None);
    }

    #[test]
    fn a_fresh_caller_gets_its_own_deadline() {
        let ring = bee_auth::KeyRing::new(bee_auth::SigningKey::generate(1));
        let minted = ring
            .mint(
                &bee_auth::MintRequest {
                    principal: bee_auth::Principal::human("u1", "U"),
                    on_behalf_of: Vec::new(),
                    scope: bee_auth::Scope::new(),
                },
                1_000,
            )
            .unwrap();
        let revocations = Arc::new(bee_auth::InMemoryRevocations::new());
        let authenticator = Authenticator::new(
            ring.verifying_keys(),
            revocations.clone(),
            Arc::new(IdGen::new(Arc::new(ManualClock::new(1_000_000)))),
            Arc::new(crate::hooks::MemoryDirectory::new()),
        );
        let token = Verifier::new(&ring.verifying_keys(), revocations.as_ref())
            .verify(&minted.text, 1_000)
            .unwrap();
        let mut cx = Cx::new("u1", PrincipalKind::Human)
            .with_deadline(Instant::now() - Duration::from_millis(1));
        let mut cx = cx.with_verified_token(&token);
        let caller = Caller::from_verified(token, minted.text, cx);

        assert!(caller.cx.check_deadline().is_err());
        let fresh = authenticator.fresh(&caller).unwrap();
        assert!(fresh.cx.check_deadline().is_ok());
        assert_eq!(fresh.cx.actor_id, caller.cx.actor_id);
        assert_eq!(fresh.cx.token_id, caller.cx.token_id);
    }

    const A_ID: &str = "01JA0000000000000000000AAA";
    const B_ID: &str = "01JB0000000000000000000BBB";

    fn repo(id: &str, full_name: &str) -> Repo {
        Repo {
            id: id.into(),
            full_name: full_name.into(),
            ..Default::default()
        }
    }

    fn token(scope: bee_auth::Scope) -> VerifiedToken {
        let ring = bee_auth::KeyRing::new(bee_auth::SigningKey::generate(1));
        let minted = ring
            .mint(
                &bee_auth::MintRequest {
                    principal: bee_auth::Principal::human("u1", "U"),
                    on_behalf_of: Vec::new(),
                    scope,
                },
                1_000,
            )
            .unwrap();
        let keys = ring.verifying_keys();
        let revoked = bee_auth::InMemoryRevocations::new();
        Verifier::new(&keys, &revoked)
            .verify(&minted.text, 1_000)
            .unwrap()
    }

    fn check(
        t: &VerifiedToken,
        r: &Repo,
        ref_name: Option<&str>,
    ) -> std::result::Result<(), AuthError> {
        authorize_repo_scope(
            t,
            r,
            |key| {
                let req = ScopeRequest::new(key, Action::Commit);
                match ref_name {
                    Some(n) => req.on_ref(n),
                    None => req,
                }
            },
            1_000,
        )
    }

    fn refused(res: std::result::Result<(), AuthError>, repo: &str) {
        let e = res.expect_err("must be refused");
        assert_eq!(e.reason, bee_auth::AuthReason::PolicyDenied);
        assert!(e.to_string().contains(repo), "{e}");
    }

    #[test]
    fn repo_scope_matches_the_full_name_like_core() {
        let a = repo(A_ID, "dev/playground");
        let b = repo(B_ID, "dev/other");
        let t = token(bee_auth::Scope::new().repos(["dev/playground"]).unwrap());
        check(&t, &a, None).unwrap();
        refused(check(&t, &b, None), "dev/other");
        // A token that names repo B by id still does not reach repo A by name or id.
        let t = token(bee_auth::Scope::new().repos([B_ID]).unwrap());
        refused(check(&t, &a, None), "dev/playground");
        let owner = token(bee_auth::Scope::new().repos(["dev/*"]).unwrap());
        check(&owner, &a, None).unwrap();
        check(&owner, &b, None).unwrap();
        refused(check(&owner, &repo(A_ID, "ops/tools"), None), "ops/tools");
    }

    #[test]
    fn repo_scope_accepts_an_exact_repo_id_and_nothing_wider() {
        let a = repo(A_ID, "dev/playground");
        let b = repo(B_ID, "dev/other");
        let t = token(bee_auth::Scope::new().repos([A_ID]).unwrap());
        check(&t, &a, None).unwrap();
        refused(check(&t, &b, None), "dev/other");
        // Prefix patterns are matched against names only: one that happens to prefix an id
        // never reaches the repo through it.
        let t = token(bee_auth::Scope::new().repos(["01JA*"]).unwrap());
        refused(check(&t, &a, None), "dev/playground");
        // The id form keeps every other dimension: a ref outside the token is refused.
        let t = token(
            bee_auth::Scope::new()
                .repos([A_ID])
                .unwrap()
                .refs(["main"])
                .unwrap(),
        );
        check(&t, &a, Some("main")).unwrap();
        refused(check(&t, &a, Some("dev")), "dev");
        // An unrestricted token covers every repo.
        let t = token(bee_auth::Scope::new());
        check(&t, &a, None).unwrap();
    }
}
