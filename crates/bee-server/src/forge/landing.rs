//! Landing for change requests: the coalescer-backed [`ServerLander`], the forge inputs to
//! land policy (reviews, checks, leases), recording lands and conflicts that did not go
//! through `LandChange` (pushes, API lands), revisions on head moves and `push.autoChange`,
//! and the merge-queue worker.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bee_auth::{MintRequest, PrincipalRef, Scope};
use bee_core::ObjectId;
use bee_forge::changes::query::ListFilter;
use bee_forge::changes::vcs::{LandIntent, Lander};
use bee_forge::changes::{Changes, NewChange, QueueStep};
use bee_forge::events::EventListener;
use bee_forge::hooks::GuardedWrite;
use bee_forge::ids::hex;
use bee_forge::{Cx, ForgeError};
use bee_policy::PrincipalKind as PolicyKind;
use bee_proto::v1::write_service_server::WriteService;
use bee_proto::v1::{self, ChangeState, ErrorReason, Event, MergeQueueState, PrincipalKind, Repo};
use tokio::sync::Notify;

use super::{ForgeWiring, api_to_forge, status_to_forge};
use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::services::write::Writes;
use crate::state::{AppState, LandInputs, RepoCtx, RequestMeta};

const NS_MERGE_QUEUE_SETTING: &str = "merge_queue_setting";
const MERGE_QUEUE_SETTING_WRITE_ATTEMPTS: usize = 16;

#[derive(Clone, Copy)]
struct RememberedQueueSetting {
    version: u64,
    required: bool,
}

fn encode_queue_setting(setting: RememberedQueueSetting) -> [u8; 9] {
    let mut out = [0; 9];
    out[..8].copy_from_slice(&setting.version.to_be_bytes());
    out[8] = u8::from(setting.required);
    out
}

fn decode_queue_setting(raw: &[u8]) -> ApiResult<RememberedQueueSetting> {
    if raw.len() != 9 || raw[8] > 1 {
        return Err(ApiError::new(
            ErrorReason::Internal,
            "invalid shared merge-queue setting record",
        ));
    }
    let version = u64::from_be_bytes(raw[..8].try_into().expect("checked record length"));
    Ok(RememberedQueueSetting {
        version,
        required: raw[8] == 1,
    })
}

/// Stores only a newer forge row version, so a stale successful read cannot overwrite a
/// concurrent setting change observed through another replica.
async fn remember_queue_setting(
    state: &AppState,
    repo_id: &str,
    setting: RememberedQueueSetting,
) -> ApiResult<RememberedQueueSetting> {
    let key = repo_id.as_bytes();
    let encoded = encode_queue_setting(setting).to_vec();
    for _ in 0..MERGE_QUEUE_SETTING_WRITE_ATTEMPTS {
        let current = state.kv.get(NS_MERGE_QUEUE_SETTING, key).await?;
        if let Some(raw) = &current {
            let current_setting = decode_queue_setting(raw)?;
            if current_setting.version >= setting.version {
                return Ok(current_setting);
            }
        }
        if state
            .kv
            .apply(vec![
                crate::kv::KvOp::expect(NS_MERGE_QUEUE_SETTING, key.to_vec(), current),
                crate::kv::KvOp::put(NS_MERGE_QUEUE_SETTING, key.to_vec(), encoded.clone()),
            ])
            .await?
        {
            return Ok(setting);
        }
    }
    Err(ApiError::new(
        ErrorReason::Unavailable,
        "shared merge-queue setting kept changing during persistence",
    ))
}

fn cache_queue_setting(
    cache: &Mutex<HashMap<String, (u64, bool)>>,
    repo_id: String,
    setting: RememberedQueueSetting,
) {
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    if cache
        .get(&repo_id)
        .is_none_or(|(version, _)| *version <= setting.version)
    {
        cache.insert(repo_id, (setting.version, setting.required));
    }
}

async fn shared_queue_setting(
    state: &AppState,
    repo_id: &str,
) -> ApiResult<Option<RememberedQueueSetting>> {
    state
        .kv
        .get(NS_MERGE_QUEUE_SETTING, repo_id.as_bytes())
        .await?
        .map(|raw| decode_queue_setting(&raw))
        .transpose()
}

/// Persists a committed repo setting before its update RPC can return success.
pub(crate) struct MergeQueueSettingsObserver {
    state: Weak<AppState>,
    cache: Arc<Mutex<HashMap<String, (u64, bool)>>>,
}

impl MergeQueueSettingsObserver {
    /// An observer over the server's shared metadata KV.
    pub(crate) fn new(
        state: &Arc<AppState>,
        cache: Arc<Mutex<HashMap<String, (u64, bool)>>>,
    ) -> Self {
        Self {
            state: Arc::downgrade(state),
            cache,
        }
    }
}

#[async_trait]
impl bee_forge::service::RepoSettingsObserver for MergeQueueSettingsObserver {
    async fn updated(&self, settings: &v1::RepoSettings) -> bee_forge::Result<()> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| ForgeError::unavailable("server stopping"))?;
        let remembered = remember_queue_setting(
            &state,
            &settings.repo_id,
            RememberedQueueSetting {
                version: settings.version,
                required: settings.merge_queue_required,
            },
        )
        .await
        .map_err(|error| {
            ForgeError::unavailable(format!(
                "repository setting committed but shared merge-queue setting write failed: {}",
                error.detail.message
            ))
        })?;
        cache_queue_setting(&self.cache, settings.repo_id.clone(), remembered);
        Ok(())
    }
}

/// Server-table namespace mapping pushed refs to the CRs they opened (`<repo>/<ref>` → id).
pub const NS_AUTO_CR: &str = "auto_cr";

/// How long internal land tokens live.
const INTERNAL_TOKEN_SECS: u64 = 300;

/// A forge context with a short deadline for calls on the land path, so a slow forge
/// database never holds a land for long.
pub fn quick_cx() -> Cx {
    let now = tokio::time::Instant::now();
    let (request_deadline, lookup) = crate::state::REQUEST_FORGE_DEADLINES
        .try_with(|(deadline, lookup)| (*deadline, *lookup))
        .unwrap_or((
            std::time::Instant::now() + Duration::from_secs(30),
            Duration::from_secs(3),
        ));
    Cx::system().with_deadline((now + lookup).min(request_deadline.into()))
}

fn policy_kind(k: &str) -> PolicyKind {
    match k {
        "agent" => PolicyKind::Agent,
        "service" => PolicyKind::Service,
        _ => PolicyKind::Human,
    }
}

/// Mints a short-lived token for a principal chain (actor first), for lands the forge
/// decided on behalf of that chain (LandChange, auto-merge, the merge queue). The token is
/// verified by the server like any other, so deactivated principals still cannot land.
pub async fn internal_token(
    state: &AppState,
    chain: &[String],
) -> crate::errors::ApiResult<String> {
    let actor = chain.first().ok_or_else(|| {
        crate::errors::ApiError::invalid("missing_field", "empty principal chain")
    })?;
    let principal = match state.principal(actor).await? {
        Some(r) => r.to_auth(),
        None => bee_auth::Principal::human(actor.clone(), actor.clone()),
    };
    let mut on_behalf_of = Vec::new();
    for id in chain.iter().skip(1) {
        let kind = state
            .principal(id)
            .await?
            .map(|r| policy_kind(&r.kind))
            .unwrap_or(PolicyKind::Human);
        on_behalf_of.push(PrincipalRef::new(id.clone(), kind));
    }
    let now = state.now_ms() / 1000;
    let token = state
        .ring
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .mint(
            &MintRequest {
                principal,
                on_behalf_of,
                scope: Scope::new().expires_at(now + INTERNAL_TOKEN_SECS),
            },
            now,
        )
        .map_err(crate::errors::ApiError::from)?;
    Ok(token.text)
}

/// Lands change requests through the server's `WriteService.Land` path: the required land
/// coalescer for protected refs, squash support, budgets, audit and the post-land hooks.
pub struct ServerLander {
    state: Weak<AppState>,
}

impl ServerLander {
    /// A lander over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerLander {
        ServerLander {
            state: Arc::downgrade(state),
        }
    }
}

#[async_trait]
impl Lander for ServerLander {
    async fn land(&self, repo: &Repo, intent: &LandIntent) -> bee_forge::Result<v1::LandResponse> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| ForgeError::unavailable("server stopping"))?;
        let token = internal_token(&state, &intent.chain)
            .await
            .map_err(api_to_forge)?;
        let mut req = tonic::Request::new(v1::LandRequest {
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::Id(repo.id.clone())),
            }),
            r#ref: intent.ref_name.clone(),
            head: Some(intent.head.clone()),
            expected_version: intent.expected_version,
            allow_empty: false,
            change_request_id: intent.change_request_id.clone(),
            method: intent.method as i32,
            message: intent.message.clone(),
            change_id: intent.change_id.clone().map(|raw| v1::ChangeId { raw }),
        });
        let md = req.metadata_mut();
        if let Ok(v) = format!("Bearer {token}").parse() {
            md.insert("authorization", v);
        }
        req.extensions_mut().insert(RequestMeta {
            request_id: crate::util::random_hex(8),
            deadline: Instant::now() + Duration::from_millis(state.cfg.limits.max_deadline_ms),
            method: "/bee.v1.ChangeService/LandChange".into(),
        });
        Writes::new(state.clone())
            .land(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_forge(&s))
    }
}

fn head_of(cr: &v1::ChangeRequest) -> Option<&v1::ObjectId> {
    cr.revisions.last().and_then(|r| r.head.as_ref())
}

fn merge_queue_required_error(ref_name: &str) -> ApiError {
    let mut error = ApiError::new(
        ErrorReason::InvalidState,
        format!(
            "{ref_name} accepts changes only through its merge queue; add the change request to the queue"
        ),
    );
    error.detail.subcode = "merge_queue_required".to_string();
    error
}

impl ForgeWiring {
    /// The forge repo row of a VCS repo, when the forge knows it.
    pub(crate) async fn forge_repo(&self, cx: &Cx, repo_id: &str) -> Option<Repo> {
        self.forge.orgs.repo_any(cx, repo_id).await.ok()
    }

    /// Resolves queue requirements while retaining the last setting across a store outage.
    pub(crate) async fn merge_queue_required(
        &self,
        state: &AppState,
        repo_ctx: &RepoCtx,
        ref_name: &str,
    ) -> ApiResult<bool> {
        if crate::services::policy::merge_queue_required(state, repo_ctx, ref_name).await? {
            return Ok(true);
        }
        let cx = quick_cx();
        let repo_key = repo_ctx.repo.id.0.to_text();
        match self.forge.orgs.repo_any(&cx, &repo_key).await {
            Ok(repo) => match self.forge.orgs.repo_settings(&cx, &repo.id).await {
                Ok(settings) => {
                    let required = settings.merge_queue_required;
                    let remembered = remember_queue_setting(
                        state,
                        &repo.id,
                        RememberedQueueSetting {
                            version: settings.version,
                            required,
                        },
                    )
                    .await?;
                    cache_queue_setting(&self.merge_queue_settings, repo_key, remembered);
                    Ok(remembered.required)
                }
                Err(error) if error.reason == ErrorReason::Unavailable => {
                    let assumed = shared_queue_setting(state, &repo_key).await?;
                    let required = assumed.map(|setting| setting.required).unwrap_or(false);
                    if let Some(setting) = assumed {
                        cache_queue_setting(&self.merge_queue_settings, repo_key.clone(), setting);
                    }
                    logging::log(
                        Level::Warn,
                        "forge",
                        "merge-queue repository setting unavailable; using shared remembered value",
                        &[
                            ("repo_id", repo_key.into()),
                            ("merge_queue_required", required.into()),
                        ],
                    );
                    Ok(required)
                }
                Err(error) => Err(super::forge_to_api(error)),
            },
            Err(error) if error.reason == ErrorReason::NotFound => Ok(false),
            Err(error) if error.reason == ErrorReason::Unavailable => {
                let assumed = shared_queue_setting(state, &repo_key).await?;
                let required = assumed.map(|setting| setting.required).unwrap_or(false);
                if let Some(setting) = assumed {
                    cache_queue_setting(&self.merge_queue_settings, repo_key.clone(), setting);
                }
                logging::log(
                    Level::Warn,
                    "forge",
                    "merge-queue repository setting unavailable; using shared remembered value",
                    &[
                        ("repo_id", repo_key.into()),
                        ("merge_queue_required", required.into()),
                    ],
                );
                Ok(required)
            }
            Err(error) => Err(super::forge_to_api(error)),
        }
    }

    /// Refuses a publication that has not reached the merge queue's landing step.
    ///
    /// This is deliberately checked immediately before the coalescer publishes, rather than
    /// trusting the route that submitted the land.  That makes native lands and a ruleset
    /// activation racing a change-request land obey the same durable queue state.
    pub(crate) async fn require_landing_queue_entry(
        &self,
        repo_ctx: &RepoCtx,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> ApiResult<()> {
        let cx = quick_cx();
        // Consult the VCS-side policy before the forge row. This is cached unless an active
        // API ruleset matches; after the cheap answer says a queue is required, the durable
        // queue entry is read below.
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| ApiError::new(ErrorReason::Unavailable, "server stopping"))?;
        let required = self
            .merge_queue_required(state.as_ref(), repo_ctx, ref_name)
            .await?;
        if !required {
            return Ok(());
        }
        let repo = match self
            .forge
            .orgs
            .repo_any(&cx, &repo_ctx.repo.id.0.to_text())
            .await
        {
            Ok(repo) => repo,
            // A VCS repository need not have a forge row.  It cannot have a repository
            // setting or queue entry, so the VCS-side ruleset answer is authoritative.
            Err(error) if error.reason == ErrorReason::NotFound => {
                if required {
                    return Err(merge_queue_required_error(ref_name));
                }
                return Ok(());
            }
            Err(error) if error.reason == ErrorReason::Unavailable => {
                let mut error = super::forge_to_api(error);
                // The retryable reason remains UNAVAILABLE (the forge must confirm a durable
                // queue entry), while the subcode tells the caller which gate blocked the ref.
                error.detail.subcode = "merge_queue_required".to_string();
                return Err(error);
            }
            Err(error) => return Err(super::forge_to_api(error)),
        };
        let queued = if change_request_id.is_empty() {
            false
        } else {
            let cr = self
                .changes
                .get(&cx, &repo, change_request_id)
                .await
                .map_err(super::forge_to_api)?;
            let queue_head = cr
                .merge_queue_entry
                .as_ref()
                .and_then(|entry| entry.speculative_head.as_ref())
                .or_else(|| {
                    cr.revisions
                        .last()
                        .and_then(|revision| revision.head.as_ref())
                });
            cr.merge_queue_entry.as_ref().is_some_and(|entry| {
                entry.r#ref == ref_name
                    && MergeQueueState::try_from(entry.state)
                        .is_ok_and(|state| state == MergeQueueState::Landing)
                    && queue_head.is_some_and(|queued_head| queued_head.raw == head.as_bytes())
            })
        };
        if queued {
            Ok(())
        } else {
            Err(merge_queue_required_error(ref_name))
        }
    }

    async fn open_crs(&self, cx: &Cx, repo: &Repo, target: &str) -> Vec<v1::ChangeRequest> {
        let f = ListFilter {
            state: ChangeState::Open,
            target_ref: target.to_string(),
            ..ListFilter::default()
        };
        match self
            .changes
            .list(cx, repo, &f, "", bee_forge::changes::store::MAX_EXAMINE)
            .await
        {
            Ok((v, _)) => v,
            Err(_) => Vec::new(),
        }
    }

    /// Reviews, checks and leases for landing `head` on `ref_name` (SPEC §7.4 forge inputs).
    /// A forge outage yields no inputs: rules that need approvals or checks then deny, and
    /// leases fail open (SPEC §15.1).
    pub(crate) async fn land_inputs(
        &self,
        repo_id: &str,
        ref_name: &str,
        head: &ObjectId,
        cr_id: &str,
    ) -> LandInputs {
        let cx = quick_cx();
        let mut out = LandInputs::default();
        let Some(repo) = self.forge_repo(&cx, repo_id).await else {
            return out;
        };
        let head_hex = hex(head.as_bytes());
        let cr = if cr_id.is_empty() {
            self.open_crs(&cx, &repo, ref_name)
                .await
                .into_iter()
                .find(|c| head_of(c).is_some_and(|h| h.raw == head.as_bytes()))
        } else {
            self.changes.get(&cx, &repo, cr_id).await.ok()
        };
        let mut check_heads = vec![head_hex.clone()];
        if let Some(cr) = &cr {
            let cur = cr.revisions.last().map(|r| r.n).unwrap_or(0);
            // The merge queue lands the combined result it tested; its checks come first.
            let combined = cr
                .merge_queue_entry
                .as_ref()
                .and_then(|e| e.speculative_head.as_ref())
                .is_some_and(|s| s.raw == head.as_bytes());
            if let Some(h) = head_of(cr) {
                let h = hex(&h.raw);
                if h != head_hex {
                    if combined {
                        check_heads.push(h);
                    } else {
                        check_heads.insert(0, h);
                    }
                }
            }
            if let Ok(reviews) = self.changes.reviews(&cx, &repo, &cr.id).await {
                for r in reviews {
                    if r.dismissed {
                        continue;
                    }
                    let Some(p) = &r.reviewer else { continue };
                    // A review of the current revision is a review of what lands (the
                    // landed head may be a squash or a rebase of it); older ones are stale.
                    let revision = if r.revision == cur {
                        head.to_text()
                    } else {
                        format!("stale:{}", r.revision)
                    };
                    let verdict = match v1::Verdict::try_from(r.verdict) {
                        Ok(v1::Verdict::Approve) => bee_policy::Verdict::Approve,
                        Ok(v1::Verdict::RequestChanges) => bee_policy::Verdict::RequestChanges,
                        _ => bee_policy::Verdict::Comment,
                    };
                    let (kind, stored) = match self.state() {
                        Some(s) => match s.principal(&p.id).await.ok().flatten() {
                            Some(rec) => (policy_kind(&rec.kind), rec.groups.clone()),
                            None => (proto_kind(p.kind), Vec::new()),
                        },
                        None => (proto_kind(p.kind), Vec::new()),
                    };
                    let mut groups = stored;
                    for g in self.team_groups(&p.id).await {
                        if !groups.contains(&g) {
                            groups.push(g);
                        }
                    }
                    out.reviews.push(bee_policy::Review {
                        reviewer: bee_policy::Actor::new(p.id.clone(), kind).with_groups(groups),
                        revision,
                        verdict,
                    });
                }
            }
        }
        out.required_workflows = self.workflow_gates(&cx, &repo, &check_heads).await;
        let mut seen = BTreeSet::new();
        for h in check_heads {
            if let Ok(results) = self.forge.checks.check_results(&cx, &repo.id, &h).await {
                for c in results {
                    if seen.insert(c.name.clone()) {
                        out.checks.push(c);
                    }
                }
            }
        }
        if let Ok(l) = self
            .forge
            .leases
            .policy_leases(&cx, &repo.id, ref_name)
            .await
        {
            out.leases = l;
        }
        out
    }

    /// After a land published changes without going through `LandChange`: every open CR
    /// on the ref whose changes all landed is recorded as landed (`cr.landed`, so issues
    /// close and leases release).
    pub(crate) async fn landed(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        resp: &v1::LandResponse,
        cr_id: &str,
    ) {
        self.touch(repo);
        // Secret scanning of what landed (alerts, not blocks), off the land's path.
        let results: Vec<v1::ObjectId> = resp
            .landed
            .iter()
            .filter(|l| !l.skipped)
            .filter_map(|l| l.result.clone())
            .collect();
        if !results.is_empty() {
            let security = self.security.clone();
            let forge = self.forge.clone();
            let repo_id = repo.id_text();
            tokio::spawn(async move {
                let cx = Cx::system();
                if let Ok(r) = forge.orgs.repo_any(&cx, &repo_id).await {
                    for id in results {
                        let _ = security.scan_changeset(&cx, &r, &id).await;
                    }
                }
            });
        }
        if !cr_id.is_empty() {
            return;
        }
        let cx = quick_cx();
        let Some(frepo) = self.forge_repo(&cx, &repo.id_text()).await else {
            return;
        };
        let landed: HashSet<Vec<u8>> = resp
            .landed
            .iter()
            .filter_map(|l| l.change_id.as_ref().map(|c| c.raw.clone()))
            .collect();
        if landed.is_empty() {
            return;
        }
        for cr in self.open_crs(&cx, &frepo, ref_name).await {
            if cr.change_ids.is_empty() || !cr.change_ids.iter().all(|c| landed.contains(&c.raw)) {
                continue;
            }
            if let Err(e) = self
                .changes
                .record_landed(&Cx::system(), &frepo, &cr.id, resp, false)
                .await
            {
                logging::log(
                    Level::Warn,
                    "forge",
                    "recording a pushed land on its change request failed",
                    &[("cr", cr.id.clone().into()), ("error", e.message.into())],
                );
            }
        }
    }

    /// After a land failed with a conflict outside `LandChange`: the open CR whose head it
    /// was holds the conflict for a resolver.
    pub(crate) async fn land_conflict(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        head: &ObjectId,
        cr_id: &str,
        conflicts: &v1::ConflictSet,
    ) {
        if !cr_id.is_empty() {
            return;
        }
        let cx = quick_cx();
        let Some(frepo) = self.forge_repo(&cx, &repo.id_text()).await else {
            return;
        };
        if let Some(cr) = self
            .open_crs(&cx, &frepo, ref_name)
            .await
            .into_iter()
            .find(|c| head_of(c).is_some_and(|h| h.raw == head.as_bytes()))
        {
            let _ = self
                .changes
                .record_conflict(&Cx::system(), &frepo, &cr.id, ref_name, conflicts.clone())
                .await;
        }
    }

    /// After a non-protected ref moved: a CR opened from that ref gets a new revision; a git
    /// push of a new branch opens a CR when the repo's `push_auto_change` is on or the push
    /// carries the option `bee.cr` (`bee.cr.title=<title>` names it).
    pub(crate) async fn ref_moved(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        target: Option<&ObjectId>,
        caller: &Caller,
        push_options: &[String],
        via_git: bool,
    ) {
        self.touch(repo);
        let Some(state) = self.state() else { return };
        let Some(target) = target else { return };
        if ref_name.starts_with("tags/")
            || ref_name.starts_with("pull/")
            || ref_name.starts_with("git/")
        {
            return;
        }
        let cx = quick_cx();
        let Some(frepo) = self.forge_repo(&cx, &repo.id_text()).await else {
            return;
        };
        let actor_cx = caller_cx(caller);
        let key = format!("{}/{ref_name}", repo.id_text());
        let head = v1::ObjectId {
            raw: target.as_bytes().to_vec(),
        };
        if let Ok(Some(id)) = state.kv.get(NS_AUTO_CR, key.as_bytes()).await {
            let id = String::from_utf8_lossy(&id).into_owned();
            if let Ok(cr) = self.changes.get(&cx, &frepo, &id).await
                && cr.state == ChangeState::Open as i32
            {
                if let Err(e) = self.changes.revise(&actor_cx, &frepo, &id, &head).await {
                    logging::log(
                        Level::Warn,
                        "forge",
                        "revising a change request after a push failed",
                        &[("cr", id.into()), ("error", e.message.into())],
                    );
                }
                return;
            }
        }
        if !via_git {
            return;
        }
        let asked = push_options
            .iter()
            .any(|o| o == "bee.cr" || o.starts_with("bee.cr="));
        let setting = self
            .forge
            .orgs
            .repo_settings(&cx, &frepo.id)
            .await
            .map(|s| s.push_auto_change)
            .unwrap_or(false);
        if !(asked || setting) || ref_name == frepo.default_ref {
            return;
        }
        if let Err(e) = self
            .forge
            .guard_write(&actor_cx, &frepo, GuardedWrite::ChangeRequest, None)
            .await
        {
            logging::log(
                Level::Info,
                "forge",
                "push.autoChange refused by moderation",
                &[("error", e.message.into())],
            );
            return;
        }
        let title = push_options
            .iter()
            .rev()
            .find_map(|o| o.strip_prefix("bee.cr.title="))
            .unwrap_or_default()
            .to_string();
        let draft = push_options.iter().any(|o| o == "bee.cr.draft");
        match self
            .changes
            .open(
                &actor_cx,
                &frepo,
                NewChange {
                    target_ref: frepo.default_ref.clone(),
                    head: Some(head),
                    title,
                    draft,
                    ..NewChange::default()
                },
            )
            .await
        {
            Ok(cr) => {
                let _ = state
                    .kv
                    .put(NS_AUTO_CR, key.as_bytes(), cr.id.into_bytes())
                    .await;
            }
            Err(e) if e.reason == ErrorReason::EmptyChange => {}
            Err(e) => logging::log(
                Level::Warn,
                "forge",
                "push.autoChange could not open a change request",
                &[
                    ("ref", ref_name.to_string().into()),
                    ("error", e.message.into()),
                ],
            ),
        }
    }
}

fn proto_kind(k: i32) -> PolicyKind {
    match PrincipalKind::try_from(k) {
        Ok(PrincipalKind::Agent) => PolicyKind::Agent,
        Ok(PrincipalKind::Service) => PolicyKind::Service,
        _ => PolicyKind::Human,
    }
}

/// A forge context for a verified token.
pub fn token_cx(token: &bee_auth::VerifiedToken) -> Cx {
    let kind = match token.principal.kind {
        PolicyKind::Agent => PrincipalKind::Agent,
        PolicyKind::Service => PrincipalKind::Service,
        PolicyKind::Human => PrincipalKind::Human,
    };
    let mut cx = Cx::new(token.principal.id.clone(), kind)
        .with_chain(token.chain().into_iter().map(|p| p.id).collect())
        .with_deadline(tokio::time::Instant::now() + Duration::from_secs(10));
    cx.token_id = token.token_id.0.clone();
    cx
}

/// A forge context for a verified server caller.
pub fn caller_cx(caller: &Caller) -> Cx {
    let mut cx = token_cx(&caller.token);
    cx.ip = caller.ip.clone();
    cx
}

// ----- merge queue ---------------------------------------------------------------------------

/// Wakes the merge-queue worker for refs whose queue changed.
pub struct QueueListener {
    pending: Arc<Mutex<BTreeSet<(String, String)>>>,
    wake: Arc<Notify>,
}

impl QueueListener {
    /// A listener feeding `pending` and waking `wake`.
    pub fn new(
        pending: Arc<Mutex<BTreeSet<(String, String)>>>,
        wake: Arc<Notify>,
    ) -> QueueListener {
        QueueListener { pending, wake }
    }
}

#[async_trait]
impl EventListener for QueueListener {
    async fn on_event(&self, event: &Event) {
        if !matches!(
            event.kind.as_str(),
            "merge_queue.entered" | "merge_queue.updated" | "check.updated" | "cr.landed"
        ) || event.repo_id.is_empty()
        {
            return;
        }
        let target = match &event.body {
            Some(v1::event::Body::Change(cr)) => cr.target_ref.clone(),
            _ => String::new(),
        };
        if let Ok(mut p) = self.pending.lock() {
            p.insert((event.repo_id.clone(), target));
        }
        self.wake.notify_one();
    }
}

/// Steps each woken merge queue ([`Changes::step_queue`]) until it is empty or its head
/// entry waits for checks (on its head, or on its combined result with the repo setting
/// `merge_queue_check_combined`); entries that cannot land are ejected with the reason.
pub async fn run_queue(
    changes: Arc<Changes>,
    forge: Arc<bee_forge::Forge>,
    pending: Arc<Mutex<BTreeSet<(String, String)>>>,
    wake: Arc<Notify>,
) {
    loop {
        wake.notified().await;
        let batch: Vec<(String, String)> = match pending.lock() {
            Ok(mut p) => std::mem::take(&mut *p).into_iter().collect(),
            Err(_) => Vec::new(),
        };
        for (repo_id, target) in batch {
            let cx = Cx::system();
            let Ok(repo) = forge.orgs.repo_any(&cx, &repo_id).await else {
                continue;
            };
            let target = if target.is_empty() {
                repo.default_ref.clone()
            } else {
                target
            };
            for _ in 0..1000 {
                match changes.step_queue(&cx, &repo, &target).await {
                    // Landed, or ejected with the reason: go on with the next entry.
                    Ok(QueueStep::Landed(_) | QueueStep::Ejected(..)) => {}
                    Ok(QueueStep::Empty | QueueStep::Waiting) => break,
                    Err(e) if !e.retryable() => {}
                    Err(_) => break,
                }
            }
        }
    }
}
