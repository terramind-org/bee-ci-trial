//! `AuthService` (SPEC §9.1, §9.2): login with service secrets or a pasted token, `WhoAmI`
//! with the effective scope, budget and chain, access checks, agents, attenuated tokens and
//! cascading revocation.

use bee_forge::safety::audit_actions; // Shared with the Safety audit consumer.
use std::sync::Arc;

use bee_auth::{
    Attenuation, Budget, BudgetKey, MintRequest, PrincipalRef, Scope, SecretHash, ServiceSecret,
    TokenId,
};
use bee_policy::{Action, PrincipalKind};
use bee_proto::v1::{self, auth_service_server::AuthService};
use serde_json::json;

use crate::auth::{
    Caller, NS_PRINCIPAL, NS_SERVICE, NS_TOKEN, PrincipalRecord, ServiceRecord, TokenRecord,
};
use crate::convert::id_from;
use crate::errors::{ApiError, ApiResult};
use crate::kv::Kv;
use crate::services::Rpc;
use crate::services::object::{access_class, finish};
use crate::services::support;
use crate::state::{AppState, RequestMeta};
use crate::util::timestamp_secs;

/// The longest owner chain (the agent itself, its owner, theirs, ... the root human or
/// service) whose agent may still create an agent. `AppState::principal_chain` follows at most
/// eight principals; capping creators at seven keeps every chain at seven or fewer, so that walk
/// always reaches the root and a truncated chain can never pass for a whole one.
const MAX_CREATOR_CHAIN: usize = 7;

/// The auth service.
pub struct AuthApi {
    state: Arc<AppState>,
}

impl AuthApi {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> AuthApi {
        AuthApi { state }
    }
}

/// The wire form of a principal record.
pub fn principal_to(r: &PrincipalRecord) -> v1::Principal {
    v1::Principal {
        id: r.id.clone(),
        kind: match r.policy_kind() {
            PrincipalKind::Human => v1::PrincipalKind::Human,
            PrincipalKind::Agent => v1::PrincipalKind::Agent,
            PrincipalKind::Service => v1::PrincipalKind::Service,
        } as i32,
        display: r.display.clone(),
        owner_id: r.owner.clone(),
        runtime: r.runtime.clone(),
        external_ids: r.external_ids.clone(),
    }
}

fn kind_name(k: PrincipalKind) -> &'static str {
    k.as_str()
}

/// The wire form of a scope.
pub fn scope_to(s: &Scope) -> v1::TokenScope {
    v1::TokenScope {
        repos: s.repos.iter().flatten().map(ToString::to_string).collect(),
        read_paths: s
            .read_paths
            .iter()
            .flatten()
            .map(|p| format!("/{p}").into_bytes())
            .collect(),
        write_paths: s
            .write_paths
            .iter()
            .flatten()
            .map(|p| format!("/{p}").into_bytes())
            .collect(),
        refs: s.refs.iter().flatten().map(ToString::to_string).collect(),
        actions: s
            .actions
            .iter()
            .flatten()
            .map(|a| a.as_str().to_string())
            .collect(),
        expires_at: s.expires_at.map(timestamp_secs),
        budget: s.budget.map(|b| v1::Budget {
            max_changesets: b.max_changesets.unwrap_or(0),
            max_landed: b.max_landed.unwrap_or(0),
            max_upload_bytes: b.max_upload_bytes.unwrap_or(0),
        }),
        session: s.session.clone().unwrap_or_default(),
    }
}

fn bad(e: bee_auth::AuthError) -> ApiError {
    ApiError::invalid("invalid_scope", e.message)
}

/// A scope from the wire (empty lists inherit).
pub fn scope_from(t: &v1::TokenScope) -> ApiResult<Scope> {
    let mut s = Scope::new();
    if !t.repos.is_empty() {
        s = s.repos(&t.repos).map_err(bad)?;
    }
    let text = |v: &[Vec<u8>]| -> ApiResult<Vec<String>> {
        v.iter()
            .map(|p| {
                String::from_utf8(p.clone())
                    .map_err(|_| ApiError::invalid("invalid_path", "paths must be UTF-8"))
            })
            .collect()
    };
    if !t.read_paths.is_empty() {
        s = s.read_paths(text(&t.read_paths)?).map_err(bad)?;
    }
    if !t.write_paths.is_empty() {
        s = s.write_paths(text(&t.write_paths)?).map_err(bad)?;
    }
    if !t.refs.is_empty() {
        s = s.refs(&t.refs).map_err(bad)?;
    }
    if !t.actions.is_empty() {
        let mut acts = Vec::new();
        for a in &t.actions {
            acts.push(Action::parse(a).ok_or_else(|| {
                ApiError::invalid("invalid_action", format!("unknown action {a:?}"))
            })?);
        }
        s = s.actions(acts);
    }
    if let Some(e) = &t.expires_at {
        s = s.expires_at(crate::util::ts_secs(e));
    }
    if let Some(b) = &t.budget {
        let nz = |v: u64| (v > 0).then_some(v);
        s = s.budget(Budget {
            max_changesets: nz(b.max_changesets),
            max_landed: nz(b.max_landed),
            max_upload_bytes: nz(b.max_upload_bytes),
        });
    }
    if !t.session.is_empty() {
        s = s.session(t.session.clone());
    }
    Ok(s)
}

/// Stores a minted-token record.
pub async fn record_token(kv: &dyn Kv, rec: &TokenRecord) -> ApiResult<()> {
    kv.put(
        NS_TOKEN,
        rec.token_id.as_bytes(),
        serde_json::to_vec(rec).unwrap_or_default(),
    )
    .await?;
    Ok(())
}

impl AuthApi {
    async fn login_impl(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::LoginRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::LoginResponse> {
        let s = &self.state;
        match req.method {
            Some(v1::login_request::Method::Service(cred)) => {
                let rec: Option<ServiceRecord> =
                    s.kv.get(NS_SERVICE, cred.name.as_bytes())
                        .await?
                        .and_then(|b| serde_json::from_slice(&b).ok());
                let presented = ServiceSecret::new(cred.secret);
                let ok = match &rec {
                    Some(r) => {
                        SecretHash::from_hex(&r.secret_sha256).is_ok_and(|h| h.matches(&presented))
                    }
                    None => {
                        // Unknown names cost the same comparison.
                        let _ = SecretHash::of(b"").matches(&presented);
                        false
                    }
                };
                let Some(rec) = rec.filter(|_| ok) else {
                    return Err(ApiError::unauthenticated("unknown service or wrong secret"));
                };
                let principal = s
                    .principal(&rec.principal)
                    .await?
                    .ok_or_else(|| ApiError::unauthenticated("service principal missing"))?;
                if principal.deactivated {
                    return Err(ApiError::unauthenticated(
                        "service principal is deactivated",
                    ));
                }
                let now = s.now_ms() / 1000;
                let exp = now + s.cfg.auth.token_lifetime_secs;
                let token = s
                    .ring
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .mint(
                        &MintRequest {
                            principal: principal.to_auth(),
                            on_behalf_of: Vec::new(),
                            scope: Scope::new().expires_at(exp),
                        },
                        now,
                    )
                    .map_err(ApiError::from)?;
                record_token(
                    s.kv.as_ref(),
                    &TokenRecord {
                        token_id: token.id.0.clone(),
                        principal: principal.id.clone(),
                        parent: String::new(),
                        on_behalf_of: Vec::new(),
                        created_ms: s.now_ms(),
                        expires_at: exp,
                        description: format!("service login {}", cred.name),
                        scope: None,
                    },
                )
                .await?;
                s.audit
                    .record(crate::audit::AuditEntry {
                        at_ms: s.now_ms(),
                        chain: vec![principal.id.clone()],
                        action: meta.method.clone(),
                        target: token.id.0.clone(),
                        decision: "service_secret".into(),
                        request_id: meta.request_id.clone(),
                        ..Default::default()
                    })
                    .await?;
                Ok(v1::LoginResponse {
                    token: token.text,
                    expires_at: Some(timestamp_secs(exp)),
                    ..Default::default()
                })
            }
            Some(v1::login_request::Method::OidcIdToken(t))
                if crate::login::is_jwt(t.trim()) && !s.cfg.login.oidc_issuer.is_empty() =>
            {
                crate::login::oidc_login(s, t.trim(), meta).await
            }
            Some(v1::login_request::Method::GithubToken(t)) => {
                crate::login::github_exchange(s, &t, meta).await
            }
            Some(v1::login_request::Method::OidcIdToken(t)) => {
                // Token paste: a bee token presented here is verified and confirmed, so a
                // client can store it. It is never extended or re-minted.
                let (v, _) = s.verify_token(t.trim()).await.map_err(|_| {
                    ApiError::unauthenticated(
                        "OIDC is not configured on this server; paste a bee token instead",
                    )
                })?;
                let _ = md;
                // A human with an authenticator answers a second-factor challenge first.
                let elevated = v
                    .scope
                    .session
                    .as_deref()
                    .is_some_and(|x| x.starts_with("2fa:"));
                if v.principal.kind == PrincipalKind::Human
                    && v.on_behalf_of.is_empty()
                    && !elevated
                    && crate::services::second_factor::has_totp(s.kv.as_ref(), &v.principal.id)
                        .await?
                {
                    return crate::services::second_factor::challenge(
                        s,
                        &v.principal.id,
                        v.scope.expires_at.unwrap_or(0),
                    )
                    .await;
                }
                Ok(v1::LoginResponse {
                    token: t.trim().to_string(),
                    expires_at: v.scope.expires_at.map(timestamp_secs),
                    ..Default::default()
                })
            }
            Some(v1::login_request::Method::GithubStart(_)) => crate::login::github_start(s).await,
            Some(v1::login_request::Method::GithubDeviceCode(code)) => {
                crate::login::github_poll(s, &code, meta).await
            }
            None => Err(ApiError::invalid(
                "missing_field",
                "a login method is required",
            )),
        }
    }

    async fn whoami_impl(&self, caller: &Caller) -> ApiResult<v1::WhoAmIResponse> {
        let s = &self.state;
        let record = caller.record.clone().unwrap_or_else(|| PrincipalRecord {
            id: caller.id().to_string(),
            kind: kind_name(caller.token.principal.kind).to_string(),
            display: caller.id().to_string(),
            org: String::new(),
            owner: String::new(),
            runtime: String::new(),
            purpose: String::new(),
            external_ids: Vec::new(),
            groups: Vec::new(),
            deactivated: false,
        });
        // Remaining budget: the tightest of every budgeted block and the delegation root's
        // quota.
        let mut remaining: Option<(Option<u64>, Option<u64>, Option<u64>)> = None;
        let mut fold = |limit: &Budget, used: crate::auth::Usage| {
            if limit.max_changesets.is_none()
                && limit.max_landed.is_none()
                && limit.max_upload_bytes.is_none()
            {
                return;
            }
            let r = remaining.get_or_insert((None, None, None));
            let m = |cur: Option<u64>, l: Option<u64>, u: u64| match (cur, l) {
                (c, None) => c,
                (None, Some(l)) => Some(l.saturating_sub(u)),
                (Some(c), Some(l)) => Some(c.min(l.saturating_sub(u))),
            };
            r.0 = m(r.0, limit.max_changesets, used.changesets);
            r.1 = m(r.1, limit.max_landed, used.landed);
            r.2 = m(r.2, limit.max_upload_bytes, used.upload_bytes);
        };
        for c in caller.token.budget_charges() {
            let used = s.ledger.usage(s.kv.as_ref(), &c.key).await?;
            fold(&c.limit, used);
        }
        let a = &s.cfg.admission;
        let nz = |v: u64| (v > 0).then_some(v);
        let root_limit = Budget {
            max_changesets: nz(a.root_max_changesets),
            max_landed: nz(a.root_max_landed),
            max_upload_bytes: nz(a.root_max_upload_bytes),
        };
        let root_used = s
            .ledger
            .usage(
                s.kv.as_ref(),
                &BudgetKey::DelegationRoot(caller.root_id().to_string()),
            )
            .await?;
        fold(&root_limit, root_used);
        let remaining = remaining.map(|(c, l, u)| v1::Budget {
            max_changesets: c.unwrap_or(0),
            max_landed: l.unwrap_or(0),
            max_upload_bytes: u.unwrap_or(0),
        });
        let mut groups = vec![
            caller.token.principal.kind.builtin_group().to_string(),
            "everyone".to_string(),
        ];
        groups.extend(s.groups_of(&record.id, record.groups.clone()).await);
        Ok(v1::WhoAmIResponse {
            principal: Some(principal_to(&record)),
            on_behalf_of: caller
                .token
                .on_behalf_of
                .iter()
                .map(|p| p.id.clone())
                .collect(),
            effective_scope: Some(scope_to(&caller.token.scope)),
            remaining,
            groups,
            expires_at: caller.token.scope.expires_at.map(timestamp_secs),
            token_id: caller.token.token_id.0.clone(),
        })
    }

    async fn check_access_impl(
        &self,
        caller: &Caller,
        req: v1::CheckAccessRequest,
    ) -> ApiResult<v1::CheckAccessResponse> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let action = Action::parse(&req.action).ok_or_else(|| {
            ApiError::invalid("invalid_action", format!("unknown action {:?}", req.action))
        })?;
        // The caller learns nothing about a repo it cannot read (not even that it exists).
        s.authorize_repo(caller, &repo, Action::Read).await?;
        let gate = s.gate(caller, &repo, action, None).await;
        let policy = s.policy(&repo, "main").await?;
        let (epoch, _) = s.epochs(&repo, &policy).await?;
        let mut allowed = Vec::new();
        let mut classes = Vec::new();
        let anyone = bee_policy::Actor::new("anyone", PrincipalKind::Human);
        for p in &req.paths {
            let path = String::from_utf8_lossy(p)
                .trim_start_matches('/')
                .to_string();
            allowed
                .push(gate.is_ok() && s.path_allowed(caller, repo.repo.id, &policy, action, &path));
            classes.push(
                if policy.check_access(&anyone, Action::Read, &path).allowed {
                    v1::AccessClass::Root as i32
                } else {
                    v1::AccessClass::Restricted as i32
                },
            );
        }
        Ok(v1::CheckAccessResponse {
            allowed,
            policy_epoch: epoch.to_string(),
            access_classes: classes,
        })
    }

    async fn create_agent_impl(
        &self,
        caller: &Caller,
        req: v1::CreateAgentRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::Principal> {
        let s = &self.state;
        if req.display.trim().is_empty() {
            return Err(ApiError::invalid("missing_field", "display is required"));
        }
        let org = if caller.is_agent() {
            self.sub_agent_org(caller, &req.org).await?
        } else {
            self.creator_org(caller, &req.org).await?
        };
        let id = crate::util::new_ulid(s.now_ms())?.to_text();
        let rec = PrincipalRecord {
            id: id.clone(),
            kind: "agent".into(),
            display: req.display.clone(),
            org,
            owner: caller.id().to_string(),
            runtime: req.runtime.clone(),
            purpose: req.purpose.clone(),
            external_ids: Vec::new(),
            groups: Vec::new(),
            deactivated: false,
        };
        s.kv.put(
            NS_PRINCIPAL,
            id.as_bytes(),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )
        .await?;
        s.audit(
            caller,
            meta,
            None,
            &id,
            audit_actions::CREATE_AGENT_CREDENTIAL,
            json!({ "runtime": req.runtime }),
        )
        .await?;
        Ok(principal_to(&rec))
    }

    /// The org for an agent made by a human or service.  Unlike an agent's sub-agent,
    /// this may be any org in which its creator is a forge member.
    async fn creator_org(&self, caller: &Caller, requested: &str) -> ApiResult<String> {
        let s = &self.state;
        let own = s
            .principal(caller.id())
            .await?
            .map(|r| r.org)
            .unwrap_or_default();
        if requested.is_empty() {
            return Ok(own);
        }

        let denied =
            || ApiError::denied("agents may be created only in an org the caller belongs to");
        if crate::services::support::is_operator(s, caller) {
            return match support::org_by_name_or_id(s, requested).await {
                Ok(org) => Ok(org.name),
                Err(e) if e.reason() == v1::ErrorReason::NotFound => Ok(requested.into()),
                Err(e) => Err(e),
            };
        }
        if requested == own {
            return Ok(own);
        }

        let org = match support::org_by_name_or_id(s, requested).await {
            Ok(org) => org,
            Err(e) if e.reason() == v1::ErrorReason::NotFound => return Err(denied()),
            Err(e) => return Err(e),
        };
        let own_org = support::org_by_name_or_id(s, &own).await.ok();
        if own_org.is_some_and(|o| o.id == org.id) {
            return Ok(own);
        }

        let Some(wiring) = s.hub.get() else {
            return Err(denied());
        };
        let cx = bee_forge::Cx::system();
        let member = wiring
            .forge
            .orgs
            .member(&cx, &org.id.0.to_text(), caller.id())
            .await
            .map_err(crate::forge::forge_to_api)?;
        if member.is_some() {
            Ok(org.name)
        } else {
            Err(denied())
        }
    }

    /// The org of a sub-agent an agent creates (SPEC §9.1): the calling agent's own recorded
    /// org, which `requested` may leave empty or name (by name or id). Refused when the
    /// caller's owner chain is already [`MAX_CREATOR_CHAIN`] long.
    async fn sub_agent_org(&self, caller: &Caller, requested: &str) -> ApiResult<String> {
        let s = &self.state;
        // Deactivated principals never get here (`verify_token` refuses the whole chain);
        // `principal_chain` refuses one deactivated since as well.
        let chain = s.principal_chain(caller.id()).await?;
        if chain.len() >= MAX_CREATOR_CHAIN {
            return Err(ApiError::denied(format!(
                "agent {} already has an owner chain of {} principals; agents with a chain of \
                 {MAX_CREATOR_CHAIN} or more cannot create agents",
                caller.id(),
                chain.len()
            )));
        }
        let own = s
            .principal(caller.id())
            .await?
            .map(|r| r.org)
            .unwrap_or_default();
        if requested.is_empty() || requested == own {
            return Ok(own);
        }
        let same = !own.is_empty()
            && match (
                support::org_by_name_or_id(s, &own).await,
                support::org_by_name_or_id(s, requested).await,
            ) {
                (Ok(a), Ok(b)) => a.id == b.id,
                _ => false,
            };
        if same {
            return Ok(own);
        }
        Err(ApiError::denied(format!(
            "an agent creates agents only in its own org ({}); org {requested:?} is not it",
            if own.is_empty() { "none" } else { &own }
        )))
    }

    async fn mint_impl(
        &self,
        caller: &Caller,
        req: v1::MintTokenRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::MintTokenResponse> {
        let s = &self.state;
        let scope = match &req.scope {
            Some(t) => scope_from(t)?,
            None => Scope::new(),
        };
        let act_as = if req.principal_id.is_empty() || req.principal_id == caller.id() {
            None
        } else {
            let target = s
                .principal(&req.principal_id)
                .await?
                .ok_or_else(|| ApiError::not_found("principal not found"))?;
            if target.policy_kind() != PrincipalKind::Agent || target.owner != caller.id() {
                return Err(ApiError::denied(
                    "tokens can be minted only for yourself or for agents you own",
                ));
            }
            if target.deactivated {
                return Err(ApiError::denied("the agent is deactivated"));
            }
            Some(PrincipalRef::new(target.id.clone(), PrincipalKind::Agent))
        };
        // A derived token: it contains every block of the caller's token, so revoking the
        // caller's token revokes it too, and it can never exceed the caller's scope.
        let mut scope = scope;
        if scope.expires_at.is_none() {
            let default = s.now_ms() / 1000 + s.cfg.auth.token_lifetime_secs;
            scope.expires_at = Some(match caller.token.scope.expires_at {
                Some(e) => e.min(default),
                None => default,
            });
        }
        let token = bee_auth::attenuate(
            &caller.token_text,
            &Attenuation {
                scope: scope.clone(),
                act_as: act_as.clone(),
            },
        )
        .map_err(ApiError::from)?;
        let principal = act_as
            .as_ref()
            .map_or_else(|| caller.id().to_string(), |p| p.id.clone());
        let mut chain = caller.chain_ids();
        if act_as.is_none() {
            chain.remove(0);
        }
        record_token(
            s.kv.as_ref(),
            &TokenRecord {
                token_id: token.id.0.clone(),
                principal: principal.clone(),
                parent: caller.token.token_id.0.clone(),
                on_behalf_of: chain,
                created_ms: s.now_ms(),
                expires_at: scope.expires_at.unwrap_or(0),
                description: String::new(),
                scope: Some(v1::TokenScope {
                    expires_at: scope.expires_at.map(timestamp_secs),
                    ..req.scope.clone().unwrap_or_default()
                }),
            },
        )
        .await?;
        s.audit(
            caller,
            meta,
            None,
            &token.id.0,
            audit_actions::MINT_TOKEN,
            json!({ "principal": principal }),
        )
        .await?;
        Ok(v1::MintTokenResponse {
            token: token.text,
            token_id: token.id.0,
        })
    }

    async fn may_manage_token(&self, caller: &Caller, token_id: &str) -> ApiResult<bool> {
        if caller.token.block_ids().iter().any(|b| b.0 == token_id) {
            return Ok(true);
        }
        let rec: Option<TokenRecord> = self
            .state
            .kv
            .get(NS_TOKEN, token_id.as_bytes())
            .await?
            .and_then(|b| serde_json::from_slice(&b).ok());
        Ok(rec.is_some_and(|r| {
            r.principal == caller.id() || r.on_behalf_of.iter().any(|p| p == caller.id())
        }))
    }

    async fn revoke_impl(
        &self,
        caller: &Caller,
        req: v1::RevokeRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::RevokeResponse> {
        let s = &self.state;
        let id = req.token_id.trim().to_ascii_lowercase();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiError::invalid(
                "invalid_id",
                "token_id must be the hex token id",
            ));
        }
        if !self.may_manage_token(caller, &id).await? {
            return Err(ApiError::denied(
                "you may revoke only your own tokens and tokens delegated from them",
            ));
        }
        let new = s
            .revocations
            .revoke(s.kv.as_ref(), &TokenId(id.clone()), s.now_ms())
            .await?;
        s.audit(
            caller,
            meta,
            None,
            &id,
            audit_actions::REVOKE_TOKEN,
            json!({ "new": new }),
        )
        .await?;
        Ok(v1::RevokeResponse {})
    }

    async fn check_object_access_impl(
        &self,
        caller: &Caller,
        req: v1::CheckObjectAccessRequest,
    ) -> ApiResult<v1::CheckObjectAccessResponse> {
        let s = &self.state;
        let _ = caller;
        let subject_md = {
            let mut m = tonic::metadata::MetadataMap::new();
            let v = format!("Bearer {}", req.subject_token)
                .parse()
                .map_err(|_| ApiError::unauthenticated("malformed subject token"))?;
            m.insert("authorization", v);
            m
        };
        let subject = s.caller(&subject_md).await?;
        let repo = s.repo(req.repo.as_ref()).await?;
        // Access classes and epochs are only answered for a repo the subject can read (the
        // caller holds the subject's token, so it learns nothing the subject could not).
        s.authorize_repo(&subject, &repo, Action::Read).await?;
        let policy = s.policy(&repo, "main").await?;
        let gate = s.gate(&subject, &repo, Action::Read, None).await.is_ok();
        let mut allowed = Vec::new();
        let mut classes = Vec::new();
        for raw in &req.ids {
            let id = id_from(Some(raw), "ids")?;
            allowed.push(gate && s.object_readable(&subject, &repo, &policy, &id).await?);
            classes.push(access_class(s, &repo, &policy, &id).await?);
        }
        let (epoch, redaction) = s.epochs(&repo, &policy).await?;
        Ok(v1::CheckObjectAccessResponse {
            allowed,
            access_classes: classes,
            policy_epoch: epoch.to_string(),
            redaction_epoch: redaction,
            principal_id: subject.id().to_string(),
            cache_ttl_seconds: 60,
        })
    }

    async fn list_tokens_impl(
        &self,
        caller: &Caller,
        req: v1::ListTokensRequest,
    ) -> ApiResult<v1::ListTokensResponse> {
        let s = &self.state;
        let who = if req.principal_id.is_empty() {
            caller.id().to_string()
        } else {
            req.principal_id.clone()
        };
        if who != caller.id() {
            let rec = s
                .principal(&who)
                .await?
                .ok_or_else(|| ApiError::not_found("principal not found"))?;
            if rec.owner != caller.id() {
                return Err(ApiError::denied(
                    "you may list tokens of yourself and agents you own",
                ));
            }
        }
        let mut tokens = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        use bee_auth::RevocationList;
        loop {
            let page = s.kv.scan(NS_TOKEN, b"", after.as_deref(), 500).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.0.clone());
            for (_, v) in page {
                let Ok(r) = serde_json::from_slice::<TokenRecord>(&v) else {
                    continue;
                };
                if r.principal != who {
                    continue;
                }
                let revoked = s.revocations.is_revoked(&TokenId(r.token_id.clone()));
                if revoked && !req.include_revoked {
                    continue;
                }
                let used = s
                    .ledger
                    .usage(s.kv.as_ref(), &BudgetKey::Block(r.token_id.clone()))
                    .await?;
                tokens.push(v1::TokenInfo {
                    used: Some(v1::Budget {
                        max_changesets: used.changesets,
                        max_landed: used.landed,
                        max_upload_bytes: used.upload_bytes,
                    }),
                    token_id: r.token_id,
                    principal_id: r.principal,
                    parent_token_id: r.parent,
                    scope: Some(v1::TokenScope {
                        expires_at: Some(timestamp_secs(r.expires_at)),
                        ..r.scope.unwrap_or_default()
                    }),
                    created_at: Some(crate::util::timestamp_ms(r.created_ms)),
                    last_used_at: None,
                    revoked,
                    description: r.description,
                });
            }
        }
        Ok(v1::ListTokensResponse {
            tokens,
            page: Some(v1::PageResponse::default()),
        })
    }

    async fn owned_agent(&self, caller: &Caller, id: &str) -> ApiResult<PrincipalRecord> {
        let rec = self
            .state
            .principal(id)
            .await?
            .ok_or_else(|| ApiError::not_found("principal not found"))?;
        if rec.id != caller.id() && rec.owner != caller.id() {
            return Err(ApiError::denied(
                "only the principal itself or its owner may change it",
            ));
        }
        Ok(rec)
    }
}

macro_rules! auth_rpc {
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

#[tonic::async_trait]
impl AuthService for AuthApi {
    async fn login(&self, req: tonic::Request<v1::LoginRequest>) -> Rpc<v1::LoginResponse> {
        let meta = RequestMeta::of(&req);
        let (md, _, inner) = req.into_parts();
        finish(
            &meta,
            self.login_impl(&md, inner, &meta)
                .await
                .map(tonic::Response::new),
        )
    }

    async fn who_am_i(&self, req: tonic::Request<v1::WhoAmIRequest>) -> Rpc<v1::WhoAmIResponse> {
        auth_rpc!(self, req, |caller, _inner, meta| self.whoami_impl(&caller))
    }

    async fn check_access(
        &self,
        req: tonic::Request<v1::CheckAccessRequest>,
    ) -> Rpc<v1::CheckAccessResponse> {
        auth_rpc!(self, req, |caller, inner, meta| self
            .check_access_impl(&caller, inner))
    }

    async fn create_agent(
        &self,
        req: tonic::Request<v1::CreateAgentRequest>,
    ) -> Rpc<v1::Principal> {
        auth_rpc!(self, req, |caller, inner, meta| self
            .create_agent_impl(&caller, inner, &meta))
    }

    async fn mint_token(
        &self,
        req: tonic::Request<v1::MintTokenRequest>,
    ) -> Rpc<v1::MintTokenResponse> {
        auth_rpc!(self, req, |caller, inner, meta| self
            .mint_impl(&caller, inner, &meta))
    }

    async fn revoke(&self, req: tonic::Request<v1::RevokeRequest>) -> Rpc<v1::RevokeResponse> {
        auth_rpc!(self, req, |caller, inner, meta| self
            .revoke_impl(&caller, inner, &meta))
    }

    async fn check_object_access(
        &self,
        req: tonic::Request<v1::CheckObjectAccessRequest>,
    ) -> Rpc<v1::CheckObjectAccessResponse> {
        auth_rpc!(self, req, |caller, inner, meta| self
            .check_object_access_impl(&caller, inner))
    }

    async fn list_tokens(
        &self,
        req: tonic::Request<v1::ListTokensRequest>,
    ) -> Rpc<v1::ListTokensResponse> {
        auth_rpc!(self, req, |caller, inner, meta| self
            .list_tokens_impl(&caller, inner))
    }

    async fn list_signing_keys(
        &self,
        req: tonic::Request<v1::ListSigningKeysRequest>,
    ) -> Rpc<v1::ListSigningKeysResponse> {
        auth_rpc!(self, req, |caller, _inner, meta| async {
            let _ = &caller;
            let infos = self.state.key_infos();
            let signing = infos
                .iter()
                .filter(|k| k.retired_ms == 0)
                .map(|k| k.id)
                .next_back();
            Ok::<_, ApiError>(v1::ListSigningKeysResponse {
                keys: infos
                    .iter()
                    .map(|k| v1::SigningKey {
                        key_id: k.id.to_string(),
                        algorithm: "ed25519".into(),
                        public_key: k.public_key.clone(),
                        signing: Some(k.id) == signing,
                        created_at: Some(crate::util::timestamp_ms(k.created_ms)),
                        retired_at: (k.retired_ms > 0)
                            .then(|| crate::util::timestamp_ms(k.retired_ms)),
                    })
                    .collect(),
            })
        })
    }

    async fn get_principal(
        &self,
        req: tonic::Request<v1::GetPrincipalRequest>,
    ) -> Rpc<v1::Principal> {
        auth_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            let rec = self
                .state
                .principal(&inner.principal_id)
                .await?
                .ok_or_else(|| ApiError::not_found("principal not found"))?;
            Ok::<_, ApiError>(principal_to(&rec))
        })
    }

    async fn update_agent_principal(
        &self,
        req: tonic::Request<v1::UpdateAgentPrincipalRequest>,
    ) -> Rpc<v1::Principal> {
        auth_rpc!(self, req, |caller, inner, meta| async {
            let mut rec = self.owned_agent(&caller, &inner.principal_id).await?;
            if rec.policy_kind() != PrincipalKind::Agent {
                return Err(ApiError::invalid(
                    "wrong_kind",
                    "only agents are updated here",
                ));
            }
            if let Some(d) = inner.display {
                rec.display = d;
            }
            if let Some(r) = inner.runtime {
                rec.runtime = r;
            }
            if let Some(p) = inner.purpose {
                rec.purpose = p;
            }
            self.state
                .kv
                .put(
                    NS_PRINCIPAL,
                    rec.id.as_bytes(),
                    serde_json::to_vec(&rec).unwrap_or_default(),
                )
                .await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &rec.id,
                    audit_actions::UPDATE_AGENT_CREDENTIAL,
                    json!({}),
                )
                .await?;
            Ok(principal_to(&rec))
        })
    }

    async fn deactivate_principal(
        &self,
        req: tonic::Request<v1::DeactivatePrincipalRequest>,
    ) -> Rpc<v1::Principal> {
        auth_rpc!(self, req, |caller, inner, meta| async {
            let mut rec = self.owned_agent(&caller, &inner.principal_id).await?;
            rec.deactivated = true;
            self.state
                .kv
                .put(
                    NS_PRINCIPAL,
                    rec.id.as_bytes(),
                    serde_json::to_vec(&rec).unwrap_or_default(),
                )
                .await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &rec.id,
                    audit_actions::DEACTIVATE_AGENT,
                    json!({ "reason": inner.reason }),
                )
                .await?;
            Ok::<_, ApiError>(principal_to(&rec))
        })
    }

    async fn logout(&self, req: tonic::Request<v1::LogoutRequest>) -> Rpc<v1::LogoutResponse> {
        auth_rpc!(self, req, |caller, _inner, meta| async {
            let id = caller.token.token_id.clone();
            self.state
                .revocations
                .revoke(self.state.kv.as_ref(), &id, self.state.now_ms())
                .await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &id.0,
                    audit_actions::LOGOUT,
                    json!({}),
                )
                .await?;
            Ok::<_, ApiError>(v1::LogoutResponse {})
        })
    }

    async fn begin_second_factor_enrollment(
        &self,
        req: tonic::Request<v1::BeginSecondFactorEnrollmentRequest>,
    ) -> Rpc<v1::BeginSecondFactorEnrollmentResponse> {
        auth_rpc!(self, req, |caller, inner, meta| {
            crate::services::second_factor::begin(&self.state, &caller, inner, &meta)
        })
    }

    async fn finish_second_factor_enrollment(
        &self,
        req: tonic::Request<v1::FinishSecondFactorEnrollmentRequest>,
    ) -> Rpc<v1::SecondFactor> {
        auth_rpc!(self, req, |caller, inner, meta| {
            crate::services::second_factor::finish(&self.state, &caller, inner, &meta)
        })
    }

    async fn list_second_factors(
        &self,
        req: tonic::Request<v1::ListSecondFactorsRequest>,
    ) -> Rpc<v1::ListSecondFactorsResponse> {
        auth_rpc!(self, req, |caller, _inner, meta| {
            crate::services::second_factor::list(&self.state, &caller)
        })
    }

    async fn delete_second_factor(
        &self,
        req: tonic::Request<v1::DeleteSecondFactorRequest>,
    ) -> Rpc<v1::DeleteSecondFactorResponse> {
        auth_rpc!(self, req, |caller, inner, meta| {
            crate::services::second_factor::delete(&self.state, &caller, inner, &meta)
        })
    }

    async fn verify_second_factor(
        &self,
        req: tonic::Request<v1::VerifySecondFactorRequest>,
    ) -> Rpc<v1::VerifySecondFactorResponse> {
        // The challenge id is the credential: no bearer token.
        let meta = RequestMeta::of(&req);
        let inner = req.into_inner();
        finish(
            &meta,
            crate::services::second_factor::verify(&self.state, inner, &meta)
                .await
                .map(tonic::Response::new),
        )
    }
}
