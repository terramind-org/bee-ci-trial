//! Delegated Biscuit tokens: mint, attenuate offline, verify, authorize.
//!
//! Each block carries its scope twice:
//!
//! - as Datalog `reject if` / `check if` rules, so any Biscuit verifier enforces it, and
//! - as JSON in the block's signed `context`, so the server can report the effective scope
//!   (the intersection of every block) and account budgets per block.
//!
//! Verification requires both to agree with the request: the typed check over the effective
//! scope and the Datalog run must each allow it. A block without a valid bee context is
//! rejected, so a holder cannot hide a restriction from the effective scope.

use std::collections::HashMap;
use std::time::Duration;

use bee_policy::{Action, Actor, PathMode, PrincipalKind, RefPattern};
use biscuit_auth::builder::{Fact, Term};
use biscuit_auth::{
    AuthorizerBuilder, AuthorizerLimits, Biscuit, BiscuitBuilder, BlockBuilder, UnverifiedBiscuit,
    error,
};
use serde::{Deserialize, Serialize};

use crate::budget::{BudgetCharge, BudgetKey};
use crate::error::{AuthError, AuthReason};
use crate::keys::{KeyRing, VerifyingKeys};
use crate::principal::{Principal, PrincipalRef};
use crate::revocation::{RevocationList, TokenId};
use crate::scope::{Budget, Request, Scope, ScopeWire, request_path};

/// Default token lifetime: 12 hours.
pub const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 12 * 60 * 60;

const CONTEXT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockContext {
    v: u32,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    principal: Option<PrincipalRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    on_behalf_of: Vec<PrincipalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    act_as: Option<PrincipalRef>,
    scope: ScopeWire,
}

/// What to mint.
#[derive(Debug, Clone)]
pub struct MintRequest {
    /// Who the token acts as.
    pub principal: Principal,
    /// The delegation chain back to a human or service, nearest first. Required for agents,
    /// and its first entry must be the agent's owner.
    pub on_behalf_of: Vec<PrincipalRef>,
    /// The token's scope. Without `expires_at`, the token expires after
    /// [`DEFAULT_TOKEN_LIFETIME_SECS`].
    pub scope: Scope,
}

/// An offline attenuation: a narrower scope and optionally a sub-agent to act as.
#[derive(Debug, Clone, Default)]
pub struct Attenuation {
    /// Restrictions added by this block.
    pub scope: Scope,
    /// A sub-agent the derived token acts as. The current principal moves into
    /// `on_behalf_of`. Only agents can be named.
    pub act_as: Option<PrincipalRef>,
}

/// A minted or derived token in text form (URL-safe base64), usable as a bearer token or an
/// HTTP Basic password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The token text.
    pub text: String,
    /// The token id (revocation id of its last block).
    pub id: TokenId,
}

fn token_error(e: error::Token) -> AuthError {
    match e {
        error::Token::FailedLogic(l) => AuthError::new(
            AuthReason::PolicyDenied,
            format!("token checks failed: {l}"),
        ),
        error::Token::RunLimit(l) => AuthError::new(
            AuthReason::PolicyDenied,
            format!("token checks exceeded limits: {l}"),
        ),
        other => AuthError::new(
            AuthReason::Unauthenticated,
            format!("invalid token: {other}"),
        ),
    }
}

fn limits() -> AuthorizerLimits {
    AuthorizerLimits {
        max_facts: 1000,
        max_iterations: 100,
        max_time: Duration::from_millis(500),
    }
}

/// Datalog for one block's scope: a list of (source, parameters).
fn scope_datalog(scope: &Scope) -> Vec<(String, HashMap<String, Term>)> {
    let mut out = Vec::new();
    let mut add_patterns = |fact: &str, set: &Option<std::collections::BTreeSet<RefPattern>>| {
        let Some(set) = set else { return };
        if set
            .iter()
            .any(|p| matches!(p, RefPattern::Prefix(x) if x.is_empty()))
        {
            return;
        }
        let mut params = HashMap::new();
        let mut exprs = Vec::new();
        for (i, p) in set.iter().enumerate() {
            let name = format!("p{i}");
            match p {
                RefPattern::Exact(e) => {
                    exprs.push(format!("$v != {{{name}}}"));
                    params.insert(name, Term::Str(e.clone()));
                }
                RefPattern::Prefix(pre) => {
                    exprs.push(format!("!$v.starts_with({{{name}}})"));
                    params.insert(name, Term::Str(pre.clone()));
                }
            }
        }
        let tail = if exprs.is_empty() {
            String::new()
        } else {
            format!(", {}", exprs.join(", "))
        };
        out.push((format!("reject if {fact}($v){tail}"), params));
    };
    add_patterns("repo", &scope.repos);
    add_patterns("ref", &scope.refs);

    let mut add_prefixes = |fact: &str, set: &Option<std::collections::BTreeSet<String>>| {
        let Some(set) = set else { return };
        if set.iter().any(String::is_empty) {
            return;
        }
        let mut params = HashMap::new();
        let mut exprs = Vec::new();
        for (i, p) in set.iter().enumerate() {
            let (eq, sub) = (format!("e{i}"), format!("s{i}"));
            exprs.push(format!("$v != {{{eq}}}, !$v.starts_with({{{sub}}})"));
            params.insert(eq, Term::Str(p.clone()));
            params.insert(sub, Term::Str(format!("{p}/")));
        }
        let tail = if exprs.is_empty() {
            String::new()
        } else {
            format!(", {}", exprs.join(", "))
        };
        out.push((format!("reject if {fact}($v){tail}"), params));
    };
    add_prefixes("read_path", &scope.read_paths);
    add_prefixes("write_path", &scope.write_paths);

    if let Some(actions) = &scope.actions {
        let mut params = HashMap::new();
        let mut exprs = Vec::new();
        for (i, a) in actions.iter().enumerate() {
            let name = format!("a{i}");
            exprs.push(format!("$v != {{{name}}}"));
            params.insert(name, Term::Str(a.as_str().to_string()));
        }
        let tail = if exprs.is_empty() {
            String::new()
        } else {
            format!(", {}", exprs.join(", "))
        };
        out.push((format!("reject if operation($v){tail}"), params));
    }
    if let Some(exp) = scope.expires_at {
        let mut params = HashMap::new();
        params.insert("exp".to_string(), Term::Date(exp));
        out.push(("check if time($t), $t < {exp}".to_string(), params));
    }
    out
}

fn block_builder(scope: &Scope, context: &BlockContext) -> Result<BlockBuilder, AuthError> {
    let json = serde_json::to_string(context).map_err(|e| {
        AuthError::new(
            AuthReason::InvalidArgument,
            format!("cannot encode scope: {e}"),
        )
    })?;
    let mut b = BlockBuilder::new().context(json);
    for (src, params) in scope_datalog(scope) {
        b = b
            .code_with_params(src, params, HashMap::new())
            .map_err(|e| {
                AuthError::new(
                    AuthReason::InvalidArgument,
                    format!("cannot build token block: {e}"),
                )
            })?;
    }
    Ok(b)
}

fn str_fact(name: &str, value: &str) -> Fact {
    Fact::new(name.to_string(), vec![Term::Str(value.to_string())])
}

impl KeyRing {
    /// Mints a token signed by the current signing key, with its key id.
    pub fn mint(&self, req: &MintRequest, now: u64) -> Result<Token, AuthError> {
        req.principal.validate()?;
        let bad = |m: String| Err(AuthError::new(AuthReason::InvalidArgument, m));
        if let Some(info) = &req.principal.agent {
            match req.on_behalf_of.first() {
                Some(first) if first.id == info.owner.as_str() => {}
                _ => {
                    return bad(format!(
                        "an agent token must be minted on behalf of its owner {}",
                        info.owner
                    ));
                }
            }
        }
        if req
            .on_behalf_of
            .iter()
            .any(|p| p.id == req.principal.id.as_str())
        {
            return bad("a principal cannot act on behalf of itself".into());
        }
        let mut scope = req.scope.clone();
        let exp = scope
            .expires_at
            .unwrap_or(now.saturating_add(DEFAULT_TOKEN_LIFETIME_SECS));
        if exp <= now {
            return bad("expiry is in the past".into());
        }
        scope.expires_at = Some(exp);
        let context = BlockContext {
            v: CONTEXT_VERSION,
            kind: "authority".into(),
            principal: Some(req.principal.to_ref()),
            on_behalf_of: req.on_behalf_of.clone(),
            act_as: None,
            scope: scope.to_wire(),
        };
        let block = block_builder(&scope, &context)?;
        let signer = self.signer();
        let mut builder = BiscuitBuilder::new()
            .merge(block)
            .root_key_id(signer.id())
            .fact(str_fact("principal", req.principal.id.as_str()))
            .map_err(token_error)?;
        for p in &req.on_behalf_of {
            builder = builder
                .fact(str_fact("on_behalf_of", &p.id))
                .map_err(token_error)?;
        }
        let biscuit = builder.build(signer.keypair()).map_err(token_error)?;
        token_of(&biscuit)
    }
}

fn token_of(b: &Biscuit) -> Result<Token, AuthError> {
    let text = b.to_base64().map_err(token_error)?;
    let ids = b.revocation_identifiers();
    let last = ids
        .last()
        .ok_or_else(|| AuthError::new(AuthReason::Unauthenticated, "token has no blocks"))?;
    Ok(Token {
        text,
        id: TokenId::from_bytes(last),
    })
}

/// Derives a narrower token offline, without any key or server call. The result can never
/// allow more than `token` does: the server intersects every block.
pub fn attenuate(token: &str, att: &Attenuation) -> Result<Token, AuthError> {
    if let Some(p) = &att.act_as
        && p.kind != PrincipalKind::Agent
    {
        return Err(AuthError::new(
            AuthReason::InvalidArgument,
            "only an agent can be named in act_as",
        ));
    }
    let unverified = UnverifiedBiscuit::from_base64(token).map_err(token_error)?;
    let context = BlockContext {
        v: CONTEXT_VERSION,
        kind: "attenuation".into(),
        principal: None,
        on_behalf_of: Vec::new(),
        act_as: att.act_as.clone(),
        scope: att.scope.to_wire(),
    };
    let block = block_builder(&att.scope, &context)?;
    let derived = unverified.append(block).map_err(token_error)?;
    let text = derived.to_base64().map_err(token_error)?;
    let id = UnverifiedBiscuit::from_base64(&text)
        .map_err(token_error)?
        .revocation_identifiers()
        .last()
        .map(|b| TokenId::from_bytes(b))
        .ok_or_else(|| AuthError::new(AuthReason::Unauthenticated, "token has no blocks"))?;
    Ok(Token { text, id })
}

/// A token that passed signature, revocation, context and expiry checks.
#[derive(Debug, Clone)]
pub struct VerifiedToken {
    /// This token's id.
    pub token_id: TokenId,
    /// The id of the root token it was derived from.
    pub root_token_id: TokenId,
    /// Key id of the signing key.
    pub key_id: u32,
    /// Who is acting.
    pub principal: PrincipalRef,
    /// Delegators, nearest first, ending at the delegation root.
    pub on_behalf_of: Vec<PrincipalRef>,
    /// The effective scope: the intersection of every block.
    pub scope: Scope,
    block_ids: Vec<TokenId>,
    block_budgets: Vec<Option<Budget>>,
    biscuit: Biscuit,
}

/// Verifies tokens against public keys and a revocation list.
pub struct Verifier<'a> {
    keys: &'a VerifyingKeys,
    revocations: &'a dyn RevocationList,
}

impl<'a> Verifier<'a> {
    /// A verifier.
    pub fn new(keys: &'a VerifyingKeys, revocations: &'a dyn RevocationList) -> Verifier<'a> {
        Verifier { keys, revocations }
    }

    /// Verifies a token at time `now` (unix seconds): signature and key id, revocation of any
    /// block, every block's context, expiry, and the token's own Datalog checks.
    pub fn verify(&self, token: &str, now: u64) -> Result<VerifiedToken, AuthError> {
        let keys = self.keys;
        let biscuit = Biscuit::from_base64(token, |kid: Option<u32>| {
            keys.get(kid).ok_or(error::Format::UnknownPublicKey)
        })
        .map_err(token_error)?;
        let key_id = biscuit
            .root_key_id()
            .ok_or_else(|| AuthError::new(AuthReason::Unauthenticated, "token has no key id"))?;

        let block_ids: Vec<TokenId> = biscuit
            .revocation_identifiers()
            .iter()
            .map(|b| TokenId::from_bytes(b))
            .collect();
        if let Some(revoked) = block_ids.iter().find(|id| self.revocations.is_revoked(id)) {
            let which = if Some(revoked) == block_ids.last() {
                "token"
            } else {
                "a token it derives from"
            };
            return Err(AuthError::new(
                AuthReason::TokenRevoked,
                format!("the {which} was revoked"),
            ));
        }

        let malformed = |m: String| AuthError::new(AuthReason::Unauthenticated, m);
        let contexts = biscuit.context();
        let mut principal: Option<PrincipalRef> = None;
        let mut on_behalf_of: Vec<PrincipalRef> = Vec::new();
        let mut effective = Scope::new();
        let mut block_budgets = Vec::new();
        for (i, ctx) in contexts.iter().enumerate() {
            let raw = ctx
                .as_ref()
                .ok_or_else(|| malformed(format!("block {i} has no bee scope")))?;
            let c: BlockContext = serde_json::from_str(raw)
                .map_err(|e| malformed(format!("block {i} has an invalid bee scope: {e}")))?;
            if c.v != CONTEXT_VERSION {
                return Err(malformed(format!(
                    "block {i} has unsupported scope version {}",
                    c.v
                )));
            }
            let scope = Scope::from_wire(&c.scope)
                .map_err(|e| malformed(format!("block {i}: {}", e.message)))?;
            match (i, c.kind.as_str()) {
                (0, "authority") => {
                    let p = c
                        .principal
                        .ok_or_else(|| malformed("authority block names no principal".into()))?;
                    principal = Some(p);
                    on_behalf_of = c.on_behalf_of;
                }
                (n, "attenuation") if n > 0 => {
                    if c.principal.is_some() || !c.on_behalf_of.is_empty() {
                        return Err(malformed(format!("block {i} may not set the principal")));
                    }
                    if let Some(sub) = c.act_as {
                        if sub.kind != PrincipalKind::Agent {
                            return Err(malformed(format!("block {i} acts as a non-agent")));
                        }
                        if let Some(prev) = principal.take() {
                            on_behalf_of.insert(0, prev);
                        }
                        principal = Some(sub);
                    }
                }
                _ => {
                    return Err(malformed(format!(
                        "block {i} has unexpected kind `{}`",
                        c.kind
                    )));
                }
            }
            block_budgets.push(scope.budget);
            effective = effective.intersect(&scope);
        }
        let principal = principal.ok_or_else(|| malformed("token names no principal".into()))?;
        match effective.expires_at {
            Some(exp) if now < exp => {}
            Some(_) => return Err(AuthError::new(AuthReason::TokenExpired, "token expired")),
            None => return Err(malformed("token has no expiry".into())),
        }

        let vt = VerifiedToken {
            token_id: block_ids
                .last()
                .cloned()
                .unwrap_or_else(|| TokenId(String::new())),
            root_token_id: block_ids
                .first()
                .cloned()
                .unwrap_or_else(|| TokenId(String::new())),
            key_id,
            principal,
            on_behalf_of,
            scope: effective,
            block_ids,
            block_budgets,
            biscuit,
        };
        vt.run_datalog(None, now)?;
        Ok(vt)
    }
}

impl VerifiedToken {
    /// The principal at the root of the delegation chain (the last delegator, or the
    /// principal itself).
    pub fn delegation_root(&self) -> &PrincipalRef {
        self.on_behalf_of.last().unwrap_or(&self.principal)
    }

    /// The acting principal followed by every delegator.
    pub fn chain(&self) -> Vec<PrincipalRef> {
        let mut v = vec![self.principal.clone()];
        v.extend(self.on_behalf_of.iter().cloned());
        v
    }

    /// The chain as policy actors; `lookup` supplies each principal's external groups and
    /// `CODEOWNERS` aliases.
    pub fn policy_chain(
        &self,
        lookup: impl Fn(&PrincipalRef) -> (Vec<String>, Vec<String>),
    ) -> Vec<Actor> {
        self.chain()
            .iter()
            .map(|p| {
                let (groups, aliases) = lookup(p);
                p.to_actor(groups, aliases)
            })
            .collect()
    }

    /// The session id from the innermost block that set one.
    pub fn session(&self) -> Option<&str> {
        self.scope.session.as_deref()
    }

    /// Authorizes a request at time `now`: the effective scope must allow it and the token's
    /// Datalog must accept it. Budgets are charged separately with
    /// [`VerifiedToken::budget_charges`].
    pub fn authorize(&self, req: &Request, now: u64) -> Result<(), AuthError> {
        if self.scope.expires_at.is_some_and(|exp| now >= exp) {
            return Err(AuthError::new(AuthReason::TokenExpired, "token expired"));
        }
        self.scope.check(req)?;
        self.run_datalog(Some(req), now)
    }

    /// Authorizes a request on one repo (SPEC §9.2). A token's `repos` may name a repo by its
    /// full name (`owner/name`) or by its id, and this is the one rule every surface applies:
    /// the request is first tried with the full name, against every pattern; it is tried with
    /// the id only when the effective scope lists that exact id ([`Scope::lists_repo_id`]).
    /// No pattern is ever matched against an id, so a prefix such as `01*` cannot reach a repo
    /// through its id text, and ids (ULIDs, no `/`) never match a name. `request` builds the
    /// request for a repo key (adding paths and ref as the call needs). The refusal returned is
    /// the one for the full name.
    pub fn authorize_repo(
        &self,
        full_name: &str,
        id: &str,
        request: impl Fn(String) -> Request,
        now: u64,
    ) -> Result<(), AuthError> {
        let by_name = self.authorize(&request(full_name.to_string()), now);
        if by_name.is_err()
            && self.scope.lists_repo_id(id)
            && self.authorize(&request(id.to_string()), now).is_ok()
        {
            return Ok(());
        }
        by_name
    }

    /// Only the Datalog half of [`VerifiedToken::authorize`]: runs the token's own checks
    /// against the request. Conformance tests use it to prove both halves agree.
    pub fn check_datalog(&self, req: &Request, now: u64) -> Result<(), AuthError> {
        self.run_datalog(Some(req), now)
    }

    fn run_datalog(&self, req: Option<&Request>, now: u64) -> Result<(), AuthError> {
        let mut b = AuthorizerBuilder::new()
            .set_limits(limits())
            .fact(Fact::new("time".to_string(), vec![Term::Date(now)]))
            .map_err(token_error)?;
        if let Some(req) = req {
            b = b
                .fact(str_fact("repo", &req.repo))
                .and_then(|b| b.fact(str_fact("operation", req.action.as_str())))
                .map_err(token_error)?;
            let fact = match req.action.path_mode() {
                PathMode::Read => Some("read_path"),
                PathMode::Write => Some("write_path"),
                PathMode::None => None,
            };
            if let Some(fact) = fact {
                for p in &req.paths {
                    b = b
                        .fact(str_fact(fact, request_path(p)))
                        .map_err(token_error)?;
                }
            }
            if let Some(r) = &req.ref_name {
                b = b.fact(str_fact("ref", r)).map_err(token_error)?;
            }
        }
        let mut authorizer = b
            .policy("allow if true")
            .map_err(token_error)?
            .build(&self.biscuit)
            .map_err(token_error)?;
        authorizer.authorize().map(|_| ()).map_err(token_error)
    }

    /// One charge per budgeted block in the chain, keyed by the block's revocation id. Charging
    /// all of them makes a sub-agent's usage count against every ancestor's budget too.
    pub fn budget_charges(&self) -> Vec<BudgetCharge> {
        self.block_ids
            .iter()
            .zip(&self.block_budgets)
            .filter_map(|(id, b)| {
                b.map(|limit| BudgetCharge {
                    key: BudgetKey::Block(id.0.clone()),
                    limit,
                })
            })
            .collect()
    }

    /// Quota charges per principal and per delegation root (SPEC §15.4), with limits from org
    /// settings.
    pub fn quota_charges(&self, per_principal: Budget, per_root: Budget) -> Vec<BudgetCharge> {
        vec![
            BudgetCharge {
                key: BudgetKey::Principal(self.principal.id.clone()),
                limit: per_principal,
            },
            BudgetCharge {
                key: BudgetKey::DelegationRoot(self.delegation_root().id.clone()),
                limit: per_root,
            },
        ]
    }

    /// Ids of every block, root first. Revoking any of them revokes this token.
    pub fn block_ids(&self) -> &[TokenId] {
        &self.block_ids
    }

    /// Whether the token allows `action` at all (ignoring paths and refs).
    pub fn allows_action(&self, action: Action) -> bool {
        self.scope
            .actions
            .as_ref()
            .is_none_or(|a| a.contains(&action))
    }
}
