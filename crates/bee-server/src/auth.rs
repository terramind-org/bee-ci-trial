//! Identity on the server: principal records, token verification, revocations, budgets and
//! per-principal / per-delegation-root rate quotas (SPEC §9.1, §9.2, §15.4).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use bee_auth::{
    Budget, BudgetCharge, BudgetKey, KeyRing, RevocationList, TokenId, VerifiedToken, Verifier,
};
use bee_policy::{Actor, PrincipalKind};
use bee_proto::v1::ErrorReason;
use serde::{Deserialize, Serialize};

use crate::errors::{ApiError, ApiResult};
use crate::kv::{Kv, KvOp};

/// Namespace of principal records.
pub const NS_PRINCIPAL: &str = "principal";
/// Namespace of service credentials.
pub const NS_SERVICE: &str = "service";
/// Namespace of revoked token ids.
pub const NS_REVOKED: &str = "revoked";
/// Namespace of counters.
pub const NS_COUNTER: &str = "counter";
/// Namespace of persisted budget usage.
pub const NS_BUDGET: &str = "budget";
/// Namespace of minted-token records.
pub const NS_TOKEN: &str = "token";
/// Namespace of org administrators (`<org id>/<principal id>`).
pub const NS_ORG_ADMIN: &str = "org_admin";

/// A stored principal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalRecord {
    /// ULID text.
    pub id: String,
    /// `human`, `agent` or `service`.
    pub kind: String,
    /// Display label.
    pub display: String,
    /// Owning org id, if any.
    #[serde(default)]
    pub org: String,
    /// Owner (agents).
    #[serde(default)]
    pub owner: String,
    /// Runtime (agents).
    #[serde(default)]
    pub runtime: String,
    /// Purpose (agents).
    #[serde(default)]
    pub purpose: String,
    /// Stable external ids.
    #[serde(default)]
    pub external_ids: Vec<String>,
    /// External groups (`org/team`).
    #[serde(default)]
    pub groups: Vec<String>,
    /// Deactivated principals cannot authenticate.
    #[serde(default)]
    pub deactivated: bool,
}

impl PrincipalRecord {
    /// The policy kind.
    pub fn policy_kind(&self) -> PrincipalKind {
        PrincipalKind::parse(&self.kind).unwrap_or(PrincipalKind::Human)
    }

    /// The `bee-auth` principal for minting.
    pub fn to_auth(&self) -> bee_auth::Principal {
        let mut p = match self.policy_kind() {
            PrincipalKind::Agent => bee_auth::Principal::agent(
                &self.id,
                &self.display,
                bee_auth::AgentInfo {
                    owner: bee_policy::PrincipalId::new(&self.owner),
                    runtime: self.runtime.clone(),
                    purpose: self.purpose.clone(),
                },
            ),
            PrincipalKind::Service => bee_auth::Principal::service(&self.id, &self.display),
            PrincipalKind::Human => bee_auth::Principal::human(&self.id, &self.display),
        };
        p.org_id = (!self.org.is_empty()).then(|| self.org.clone());
        p.external_ids = self.external_ids.clone();
        p
    }
}

/// A stored service credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// The service principal.
    pub principal: String,
    /// SHA-256 of the secret, hex.
    pub secret_sha256: String,
}

/// A record of a token minted through `MintToken` or `Login`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Token id.
    pub token_id: String,
    /// Principal it acts as.
    pub principal: String,
    /// Token it was derived from (server-side attenuation), if any.
    #[serde(default)]
    pub parent: String,
    /// Delegation chain, nearest first.
    #[serde(default)]
    pub on_behalf_of: Vec<String>,
    /// Creation time.
    pub created_ms: u64,
    /// Expiry (unix seconds).
    pub expires_at: u64,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// The scope the token was minted with (repos, paths, refs, actions, budget), served by
    /// `ListTokens`; absent for root tokens and tokens recorded before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<bee_proto::v1::TokenScope>,
}

/// Revocations: an in-memory set refreshed from the server store (the propagation bound is
/// the refresh interval), updated immediately for revocations made by this process.
#[derive(Default)]
pub struct Revocations {
    inner: RwLock<(BTreeSet<TokenId>, u64)>,
}

impl Revocations {
    /// Reloads from the store.
    pub async fn refresh(&self, kv: &dyn Kv) -> ApiResult<()> {
        let mut set = BTreeSet::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = kv.scan(NS_REVOKED, b"", after.as_deref(), 1000).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.0.clone());
            for (k, _) in page {
                set.insert(TokenId(String::from_utf8_lossy(&k).into_owned()));
            }
        }
        let counter = read_counter(kv, "revocations").await?;
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        *g = (set, counter);
        Ok(())
    }

    /// Records a revocation in the store and in memory. Returns false if already revoked.
    pub async fn revoke(&self, kv: &dyn Kv, id: &TokenId, at_ms: u64) -> ApiResult<bool> {
        for _ in 0..50 {
            if kv.get(NS_REVOKED, id.0.as_bytes()).await?.is_some() {
                self.refresh(kv).await?;
                return Ok(false);
            }
            let cur = kv.get(NS_COUNTER, b"revocations").await?;
            let n = decode_u64(cur.as_deref()) + 1;
            let ok = kv
                .apply(vec![
                    KvOp::expect(NS_COUNTER, b"revocations".to_vec(), cur),
                    KvOp::expect(NS_REVOKED, id.0.as_bytes().to_vec(), None),
                    KvOp::put(
                        NS_REVOKED,
                        id.0.as_bytes().to_vec(),
                        at_ms.to_be_bytes().to_vec(),
                    ),
                    KvOp::put(
                        NS_COUNTER,
                        b"revocations".to_vec(),
                        n.to_be_bytes().to_vec(),
                    ),
                ])
                .await?;
            if ok {
                let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
                g.0.insert(id.clone());
                g.1 = g.1.max(n);
                return Ok(true);
            }
        }
        Err(ApiError::new(
            ErrorReason::Unavailable,
            "revocation contended",
        ))
    }
}

impl RevocationList for Revocations {
    fn is_revoked(&self, id: &TokenId) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .contains(id)
    }

    fn counter(&self) -> u64 {
        self.inner.read().unwrap_or_else(|p| p.into_inner()).1
    }
}

/// Decodes a big-endian counter value (absent = 0).
pub fn decode_u64(v: Option<&[u8]>) -> u64 {
    v.and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0)
}

/// Reads a named counter.
pub async fn read_counter(kv: &dyn Kv, name: &str) -> ApiResult<u64> {
    Ok(decode_u64(
        kv.get(NS_COUNTER, name.as_bytes()).await?.as_deref(),
    ))
}

/// Increments a named counter atomically and returns the new value.
pub async fn bump_counter(kv: &dyn Kv, name: &str, by: u64) -> ApiResult<u64> {
    for _ in 0..100 {
        let cur = kv.get(NS_COUNTER, name.as_bytes()).await?;
        let n = decode_u64(cur.as_deref()) + by;
        if kv
            .apply(vec![
                KvOp::expect(NS_COUNTER, name.as_bytes().to_vec(), cur),
                KvOp::put(
                    NS_COUNTER,
                    name.as_bytes().to_vec(),
                    n.to_be_bytes().to_vec(),
                ),
            ])
            .await?
        {
            return Ok(n);
        }
    }
    Err(ApiError::new(ErrorReason::Unavailable, "counter contended"))
}

/// Usage counted against a budget key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Changesets created.
    pub changesets: u64,
    /// Changes landed.
    pub landed: u64,
    /// Bytes uploaded.
    pub upload_bytes: u64,
}

impl Usage {
    fn plus(&self, o: &Usage) -> Usage {
        Usage {
            changesets: self.changesets.saturating_add(o.changesets),
            landed: self.landed.saturating_add(o.landed),
            upload_bytes: self.upload_bytes.saturating_add(o.upload_bytes),
        }
    }

    fn over(&self, limit: &Budget) -> Option<&'static str> {
        let over = |l: Option<u64>, v: u64| l.is_some_and(|l| v > l);
        if over(limit.max_changesets, self.changesets) {
            Some("changesets")
        } else if over(limit.max_landed, self.landed) {
            Some("landed changes")
        } else if over(limit.max_upload_bytes, self.upload_bytes) {
            Some("upload bytes")
        } else {
            None
        }
    }
}

fn key_text(k: &BudgetKey) -> String {
    match k {
        BudgetKey::Block(id) => format!("block/{id}"),
        BudgetKey::Principal(p) => format!("principal/{p}"),
        BudgetKey::DelegationRoot(p) => format!("root/{p}"),
    }
}

fn has_limit(b: &Budget) -> bool {
    b.max_changesets.is_some() || b.max_landed.is_some() || b.max_upload_bytes.is_some()
}

/// Budget accounting: atomic across every key of a charge. Keys with a limit are persisted
/// in the server store (so every process sees one ledger); unlimited keys are counted in
/// memory for `WhoAmI`.
#[derive(Default)]
pub struct Ledger {
    memory: Mutex<BTreeMap<String, Usage>>,
}

impl Ledger {
    /// Applies `usage` to every key if every limit allows it (`BUDGET_EXHAUSTED` for token
    /// budgets, `QUOTA_EXCEEDED` for delegation-root and principal quotas).
    pub async fn charge(
        &self,
        kv: &dyn Kv,
        charges: &[BudgetCharge],
        usage: Usage,
    ) -> ApiResult<()> {
        if usage == Usage::default() {
            return Ok(());
        }
        let limited: Vec<&BudgetCharge> = charges.iter().filter(|c| has_limit(&c.limit)).collect();
        if !limited.is_empty() {
            let mut done = false;
            for _ in 0..100 {
                let mut ops = Vec::new();
                for c in &limited {
                    let key = key_text(&c.key);
                    let cur = kv.get(NS_BUDGET, key.as_bytes()).await?;
                    let used: Usage = cur
                        .as_deref()
                        .and_then(|b| serde_json::from_slice(b).ok())
                        .unwrap_or_default();
                    let next = used.plus(&usage);
                    if let Some(what) = next.over(&c.limit) {
                        let (reason, who) = match &c.key {
                            BudgetKey::Block(id) => (
                                ErrorReason::BudgetExhausted,
                                format!("token block {}", &id[..id.len().min(16)]),
                            ),
                            BudgetKey::Principal(p) => {
                                (ErrorReason::QuotaExceeded, format!("principal {p}"))
                            }
                            BudgetKey::DelegationRoot(p) => {
                                (ErrorReason::QuotaExceeded, format!("delegation root {p}"))
                            }
                        };
                        let mut e =
                            ApiError::new(reason, format!("the {what} budget of {who} is used up"));
                        e.detail.payload =
                            Some(bee_proto::v1::error_detail::Payload::LimitExceeded(
                                bee_proto::v1::LimitExceeded {
                                    limit: what.replace(' ', "_"),
                                    maximum: match what {
                                        "changesets" => c.limit.max_changesets,
                                        "landed changes" => c.limit.max_landed,
                                        _ => c.limit.max_upload_bytes,
                                    }
                                    .unwrap_or(0),
                                    actual: match what {
                                        "changesets" => next.changesets,
                                        "landed changes" => next.landed,
                                        _ => next.upload_bytes,
                                    },
                                },
                            ));
                        return Err(e);
                    }
                    ops.push(KvOp::expect(NS_BUDGET, key.as_bytes().to_vec(), cur));
                    ops.push(KvOp::put(
                        NS_BUDGET,
                        key.as_bytes().to_vec(),
                        serde_json::to_vec(&next).unwrap_or_default(),
                    ));
                }
                if kv.apply(ops).await? {
                    done = true;
                    break;
                }
            }
            if !done {
                return Err(ApiError::new(
                    ErrorReason::Unavailable,
                    "budget ledger contended",
                ));
            }
        }
        let mut m = self.memory.lock().unwrap_or_else(|p| p.into_inner());
        for c in charges.iter().filter(|c| !has_limit(&c.limit)) {
            let e = m.entry(key_text(&c.key)).or_default();
            *e = e.plus(&usage);
        }
        Ok(())
    }

    /// Usage recorded for a key.
    pub async fn usage(&self, kv: &dyn Kv, key: &BudgetKey) -> ApiResult<Usage> {
        let k = key_text(key);
        if let Some(b) = kv.get(NS_BUDGET, k.as_bytes()).await? {
            return Ok(serde_json::from_slice(&b).unwrap_or_default());
        }
        Ok(self
            .memory
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&k)
            .copied()
            .unwrap_or_default())
    }
}

/// The rate-limit cost of a bulk read of `ids` objects (`GetObjects`, `HasObjects`): the
/// request's token plus one more per full thousand ids (SPEC §15.4).
pub fn bulk_read_cost(ids: usize) -> u64 {
    1 + ids as u64 / 1000
}

/// Token-bucket rate limits per principal and per delegation root (SPEC §15.4). Limited
/// decisions are never cached beyond their window.
pub struct RateLimiter {
    per_principal: f64,
    per_root: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    /// Limits in requests per second (0 = unlimited); bursts are twice the rate.
    pub fn new(per_principal: u64, per_root: u64) -> RateLimiter {
        RateLimiter {
            per_principal: per_principal as f64,
            per_root: per_root as f64,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one request for `principal` and `root`; on refusal returns the quota hit and the
    /// milliseconds until a token is available.
    pub fn take(&self, principal: &str, root: &str) -> Result<(), (&'static str, u64)> {
        self.take_n(principal, root, 1)
    }

    /// Takes `cost` tokens from both buckets, or none: a refusal by either bucket leaves the
    /// other untouched. On refusal returns the quota hit and the milliseconds until `cost`
    /// tokens are available (a cost above the burst can never pass and waits for a full
    /// bucket).
    pub fn take_n(
        &self,
        principal: &str,
        root: &str,
        cost: u64,
    ) -> Result<(), (&'static str, u64)> {
        let now = Instant::now();
        let cost = cost as f64;
        let mut b = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if b.len() > 100_000 {
            b.clear();
        }
        let checks = [
            (
                format!("p/{principal}"),
                self.per_principal,
                "per_principal_rps",
            ),
            (format!("r/{root}"), self.per_root, "per_root_rps"),
        ];
        // Refill both, then decide, then deduct from both.
        for (key, rate, name) in &checks {
            if *rate <= 0.0 {
                continue;
            }
            let cap = rate * 2.0;
            let e = b.entry(key.clone()).or_insert((cap, now));
            let elapsed = now.duration_since(e.1).as_secs_f64();
            e.0 = (e.0 + elapsed * rate).min(cap);
            e.1 = now;
            if e.0 < cost {
                let need = cost.min(cap) - e.0;
                let wait = (need.max(0.0) / rate * 1000.0).ceil() as u64;
                return Err((name, wait.max(1)));
            }
        }
        for (key, rate, _) in &checks {
            if *rate > 0.0
                && let Some(e) = b.get_mut(key)
            {
                e.0 -= cost;
            }
        }
        Ok(())
    }

    /// The current state of the principal and delegation-root buckets, without taking a
    /// token: `(quota name, scope, limit per second, tokens left now)`; a limit of 0 means
    /// unlimited.
    pub fn peek(&self, principal: &str, root: &str) -> Vec<(&'static str, &'static str, f64, f64)> {
        let now = Instant::now();
        let b = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        let left = |key: String, rate: f64| -> f64 {
            if rate <= 0.0 {
                return 0.0;
            }
            let cap = rate * 2.0;
            match b.get(&key) {
                Some((tokens, at)) => {
                    (tokens + now.duration_since(*at).as_secs_f64() * rate).min(cap)
                }
                None => cap,
            }
        };
        vec![
            (
                "per_principal_rps",
                "principal",
                self.per_principal,
                left(format!("p/{principal}"), self.per_principal),
            ),
            (
                "per_root_rps",
                "delegation_root",
                self.per_root,
                left(format!("r/{root}"), self.per_root),
            ),
        ]
    }
}

/// Verified-token cache: Biscuit verification is the costliest step of authorization.
/// Entries are re-checked for revocation on every hit; expiry is checked by `authorize`.
#[derive(Default)]
pub struct TokenCache {
    map: Mutex<HashMap<[u8; 32], VerifiedToken>>,
}

impl TokenCache {
    /// Drops cached verifications after a signing-key change. A cached token must not outlive
    /// retirement of the key that signed it.
    pub fn clear(&self) {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// Verifies (or returns the cached verification of) a token.
    pub fn verify(
        &self,
        ring: &RwLock<KeyRing>,
        revocations: &Revocations,
        token: &str,
        now_secs: u64,
    ) -> ApiResult<VerifiedToken> {
        let key = *blake3::hash(token.as_bytes()).as_bytes();
        if let Some(v) = self
            .map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
        {
            if v.block_ids().iter().any(|b| revocations.is_revoked(b)) {
                return Err(ApiError::new(ErrorReason::TokenRevoked, "token revoked"));
            }
            if v.scope.expires_at.is_some_and(|e| now_secs >= e) {
                return Err(ApiError::new(ErrorReason::TokenExpired, "token expired"));
            }
            return Ok(v);
        }
        let keys = ring
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .verifying_keys();
        let v = Verifier::new(&keys, revocations)
            .verify(token, now_secs)
            .map_err(ApiError::from)?;
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 50_000 {
            m.clear();
        }
        m.insert(key, v.clone());
        Ok(v)
    }
}

/// The authenticated caller of one request.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The verified token.
    pub token: VerifiedToken,
    /// The token text (forwarded when a land is routed to another process).
    pub token_text: String,
    /// The acting principal's record (if stored).
    pub record: Option<PrincipalRecord>,
    /// The chain as policy actors, acting principal first.
    pub actors: Vec<Actor>,
    /// The client address (the peer, or the trusted `X-Forwarded-For` entry).
    pub ip: String,
    /// A request that brought no credentials and was let through on the server's anonymous
    /// read token (`crate::access::ANONYMOUS_PRINCIPAL`): repo access decides it with an
    /// empty principal chain, which reads public repos and nothing else.
    pub anonymous: bool,
    /// [`Caller::actors`] as each repo's policy sees them (with `@repo-role/<role>`, SPEC
    /// §9.4), by repo id, filled by `AppState::policy_actors` for the life of this request.
    pub repo_actors: RepoActors,
}

/// Per-request memo of the policy actors of each repo a caller touched (shared by clones of
/// the caller, so a land handed to the coalescer reuses it).
pub type RepoActors = Arc<Mutex<HashMap<bee_store::RepoId, Arc<[Actor]>>>>;

impl Caller {
    /// The acting principal id.
    pub fn id(&self) -> &str {
        &self.token.principal.id
    }

    /// The principal chain ids, acting principal first.
    pub fn chain_ids(&self) -> Vec<String> {
        self.token.chain().into_iter().map(|p| p.id).collect()
    }

    /// The delegation root id.
    pub fn root_id(&self) -> &str {
        &self.token.delegation_root().id
    }

    /// True when the acting principal is an agent.
    pub fn is_agent(&self) -> bool {
        self.token.principal.kind == PrincipalKind::Agent
    }

    /// The policy actors resolved for `repo` in this request, when `AppState::policy_actors`
    /// has resolved them.
    pub fn resolved_actors(&self, repo: bee_store::RepoId) -> Option<Arc<[Actor]>> {
        self.repo_actors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&repo)
            .cloned()
    }

    /// Records the policy actors of `repo` for the rest of the request.
    pub fn remember_actors(&self, repo: bee_store::RepoId, actors: Arc<[Actor]>) {
        self.repo_actors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(repo, actors);
    }
}

/// Shared handle type for the key ring.
pub type SharedKeyRing = Arc<RwLock<KeyRing>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::SqliteKv;

    #[tokio::test]
    async fn ledger_is_atomic_across_keys() {
        let dir = tempfile::tempdir().unwrap();
        let kv = SqliteKv::open(&dir.path().join("kv.db")).unwrap();
        let ledger = Ledger::default();
        let block = BudgetCharge {
            key: BudgetKey::Block("b1".into()),
            limit: Budget {
                max_changesets: Some(2),
                ..Budget::default()
            },
        };
        let root = BudgetCharge {
            key: BudgetKey::DelegationRoot("r".into()),
            limit: Budget::default(),
        };
        let one = Usage {
            changesets: 1,
            ..Usage::default()
        };
        ledger
            .charge(&kv, &[block.clone(), root.clone()], one)
            .await
            .unwrap();
        ledger
            .charge(&kv, &[block.clone(), root.clone()], one)
            .await
            .unwrap();
        let e = ledger
            .charge(&kv, &[block.clone(), root.clone()], one)
            .await
            .unwrap_err();
        assert_eq!(e.detail.reason, "BUDGET_EXHAUSTED");
        assert_eq!(ledger.usage(&kv, &block.key).await.unwrap().changesets, 2);
        assert_eq!(ledger.usage(&kv, &root.key).await.unwrap().changesets, 2);
    }

    #[tokio::test]
    async fn revocations_count_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let kv = SqliteKv::open(&dir.path().join("kv.db")).unwrap();
        let r = Revocations::default();
        let id = TokenId("ab".into());
        assert!(r.revoke(&kv, &id, 1).await.unwrap());
        assert!(!r.revoke(&kv, &id, 2).await.unwrap());
        assert!(r.is_revoked(&id));
        let fresh = Revocations::default();
        fresh.refresh(&kv).await.unwrap();
        assert!(fresh.is_revoked(&id));
        assert_eq!(fresh.counter(), 1);
    }

    #[test]
    fn bulk_reads_cost_one_extra_token_per_thousand_ids() {
        assert_eq!(bulk_read_cost(0), 1);
        assert_eq!(bulk_read_cost(1), 1);
        assert_eq!(bulk_read_cost(999), 1);
        assert_eq!(bulk_read_cost(1000), 2);
        assert_eq!(bulk_read_cost(2999), 3);
        assert_eq!(bulk_read_cost(5000), 6);
        // Five tokens a second, burst ten: one 1000-id read and four plain calls fit, then
        // the bucket is empty; the refusal names how long until the next bulk read fits.
        let l = RateLimiter::new(5, 0);
        assert!(l.take_n("p", "r", bulk_read_cost(1000)).is_ok());
        for _ in 0..8 {
            assert!(l.take("p", "r").is_ok());
        }
        let (name, wait) = l.take_n("p", "r", bulk_read_cost(1000)).unwrap_err();
        assert_eq!(name, "per_principal_rps");
        assert!((1..=400).contains(&wait), "{wait}");
    }

    #[test]
    fn a_refusal_by_the_root_bucket_spends_nothing_from_the_principal() {
        // Principal: 10/s (burst 20). Root: 1/s (burst 2), shared by two principals.
        let l = RateLimiter::new(10, 1);
        assert!(l.take("a", "root").is_ok());
        assert!(l.take("b", "root").is_ok());
        let (name, _) = l.take("a", "root").unwrap_err();
        assert_eq!(name, "per_root_rps");
        for _ in 0..5 {
            assert!(l.take("a", "root").is_err());
        }
        // `a` spent one token, not seven.
        let left = l
            .peek("a", "root")
            .into_iter()
            .find(|(q, ..)| *q == "per_principal_rps")
            .map(|(_, _, _, left)| left)
            .unwrap();
        assert!(left > 18.5, "{left}");
    }

    #[test]
    fn rate_limiter_refuses_with_retry_after() {
        let l = RateLimiter::new(1, 0);
        assert!(l.take("p", "r").is_ok());
        assert!(l.take("p", "r").is_ok());
        let (name, wait) = l.take("p", "r").unwrap_err();
        assert_eq!(name, "per_principal_rps");
        assert!(wait >= 1);
        assert!(l.take("other", "r").is_ok());
    }
}
