//! `AuditService` as served here: the forge's query, get, stream and chain verification of an
//! org's hash-chained log, plus SIEM streaming sinks and exports (`docs/spec/operations.md` §5).
//!
//! **Sinks** are rows of the server store (`audit_sink`). A delivery worker reads each enabled
//! sink's org log after its `delivered_seq`, keeps the entries matching its action prefixes,
//! and delivers them in sequence order, every entry with its `prev_hash` and `hash` so the
//! receiver can verify the chain:
//!
//! | kind | endpoint | body |
//! |---|---|---|
//! | `HTTPS` | `http(s)://…` | JSON lines (`application/x-ndjson`), `Authorization: Bearer <secret>` |
//! | `SPLUNK` | HEC URL | `{"event": …}` objects, `Authorization: Splunk <secret>` |
//! | `DATADOG` | logs intake URL | a JSON array of `{ddsource, service, message}`, `DD-API-KEY` |
//! | `S3` | `file:///dir` under `audit.file_roots` | one JSON-lines object per batch, written atomically |
//!
//! A failed delivery is retried with exponential backoff (`audit.sink_retry_*`); after
//! `audit.sink_failing_after` consecutive failures the sink reports `FAILING`. Delivery never
//! skips: `delivered_seq` only moves past entries that were delivered (or filtered out).
//! Credentials are org secrets of the CI vault (`credential_secret` names one) and are never
//! returned.
//!
//! **Exports** collect the matching entries oldest first, write them as JSON lines (proto3
//! JSON, one entry per line) or CSV, store the file as an immutable object of the org (an
//! upload by the requester, collected by GC like any unreferenced upload), and return a
//! signed URL plus `head_hash` (the hash of the last exported entry) for offline verification.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bee_forge::audit::{AuditDraft, org_scope};
use bee_forge::auth::{Caller as ForgeCaller, OrgPerm};
use bee_forge::ci::secrets::VaultScope;
use bee_forge::events::delivery::{Prepared, Sender};
use bee_forge::service::ForgeServices;
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::audit_service_server::AuditService;
use bee_proto::v1::{self, AuditEntry, AuditLogFilter, ErrorReason};
use bee_store::{OrgId, UploadRecord};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tonic::{Request, Response, Status};

use crate::errors::{ApiError, ApiResult};
use crate::forge::{ForgeWiring, forge_to_api};
use crate::logging::{self, Level};
use crate::services::support::{get_json, next_page, offset_page, page_of, put_json, scan_json};
use crate::state::AppState;
use crate::util::timestamp_ms;

/// Namespace of sink rows (`<id>`).
pub const NS_SINK: &str = "audit_sink";
/// Namespace of sink names (`<org id>/<name>` → id).
pub const NS_SINK_NAME: &str = "audit_sink_name";
/// Namespace of export rows (`<id>`).
pub const NS_EXPORT: &str = "audit_export";

/// A stored sink.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SinkRec {
    /// Sink id (ULID).
    pub id: String,
    /// Org id.
    pub org_id: String,
    /// Org name when created.
    pub org: String,
    /// Unique per org.
    pub name: String,
    /// `AuditSinkKind`.
    pub kind: i32,
    /// Destination.
    pub endpoint: String,
    /// Org secret holding the credential.
    pub credential_secret: String,
    /// Only actions with one of these prefixes (empty = every action).
    pub action_prefixes: Vec<String>,
    /// Delivering.
    pub enabled: bool,
    /// Last sequence delivered (or skipped by the prefix filter).
    pub delivered_seq: u64,
    /// Consecutive failed deliveries.
    pub failures: u32,
    /// Last delivery error.
    pub last_error: String,
    /// Last successful delivery.
    pub last_delivered_ms: u64,
    /// No delivery before this time (backoff).
    pub next_attempt_ms: u64,
    /// Created.
    pub created_ms: u64,
    /// Last configuration change.
    pub updated_ms: u64,
}

/// A stored export.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ExportRec {
    /// Export id.
    pub id: String,
    /// Org id.
    pub org_id: String,
    /// Requesting principal.
    pub requester: String,
    /// `AuditExportFormat`.
    pub format: i32,
    /// `AuditExportState`.
    pub state: i32,
    /// The stored file (blob id text) when ready.
    pub blob: String,
    /// Entries exported.
    pub entry_count: u64,
    /// First sequence exported.
    pub first_seq: u64,
    /// Last sequence exported.
    pub last_seq: u64,
    /// Hash of the last entry (hex).
    pub head_hash: String,
    /// Failure.
    pub error: String,
    /// Created.
    pub created_ms: u64,
}

fn fe(e: ForgeError) -> ApiError {
    forge_to_api(e)
}

fn status(rid: &str, e: ApiError) -> Status {
    e.with_request_id(rid).into_status()
}

/// `AuditService` over the forge's org logs plus sinks and exports.
pub struct AuditApi {
    base: ForgeServices,
    wiring: Arc<ForgeWiring>,
}

impl AuditApi {
    /// The service.
    pub fn new(base: ForgeServices, wiring: Arc<ForgeWiring>) -> AuditApi {
        AuditApi { base, wiring }
    }

    fn state(&self) -> ApiResult<Arc<AppState>> {
        self.wiring
            .state()
            .ok_or_else(|| ApiError::new(ErrorReason::Unavailable, "server stopping"))
    }

    fn authn<T>(&self, req: &Request<T>) -> Result<(ForgeCaller, String), Status> {
        let rid = crate::state::RequestMeta::of(req).request_id;
        let caller = self
            .wiring
            .forge
            .authn
            .authenticate(req)
            .map_err(|e| status(&rid, fe(e)))?;
        Ok((caller, rid))
    }

    /// Resolves the org of a sink or export call and authorizes `perm`.
    async fn org_cx(
        &self,
        caller: &ForgeCaller,
        org: &str,
        enterprise: &str,
        perm: OrgPerm,
    ) -> ApiResult<(bee_proto::v1::Org, Cx)> {
        if !enterprise.is_empty() {
            return Err(fe(bee_forge::service::unserved(
                "AuditService (enterprise logs)",
                "enterprise",
            )));
        }
        if org.is_empty() {
            return Err(ApiError::invalid("missing_field", "org is required"));
        }
        let f = &self.wiring.forge;
        let o = f.orgs.get_org(&caller.cx, org).await.map_err(fe)?;
        let cx = f.authz.org(caller, &o.id, perm).await.map_err(fe)?;
        Ok((o, cx))
    }

    async fn audit(&self, cx: &Cx, action: &str, org_id: &str, target: &str, after: &str) {
        let Ok(state) = self.state() else { return };
        let id = match crate::util::new_ulid(state.now_ms()) {
            Ok(u) => u.to_text(),
            Err(_) => return,
        };
        let entry = AuditDraft::new(action, org_id)
            .target_id(target)
            .after(after)
            .into_entry(cx, state.now_ms(), id);
        if let Err(e) = self.wiring.forge.audit.append(cx, entry).await {
            logging::log(
                Level::Warn,
                "audit",
                "audit append failed",
                &[("error", e.message.into())],
            );
        }
    }

    async fn sink(
        &self,
        caller: &ForgeCaller,
        id: &str,
        perm: OrgPerm,
    ) -> ApiResult<(SinkRec, Cx)> {
        let state = self.state()?;
        if id.is_empty() {
            return Err(ApiError::invalid("missing_field", "id is required"));
        }
        let rec: SinkRec = get_json(state.kv.as_ref(), NS_SINK, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("audit sink {id}")))?;
        let (_, cx) = self.org_cx(caller, &rec.org_id, "", perm).await?;
        Ok((rec, cx))
    }

    async fn sink_view(&self, rec: &SinkRec) -> v1::AuditSink {
        let head = head_seq(&self.wiring, &rec.org_id).await.unwrap_or(0);
        let state = self.state().ok();
        let now = state.as_ref().map(|s| s.now_ms()).unwrap_or(0);
        let failing_after = state
            .as_ref()
            .map(|s| s.cfg.audit.sink_failing_after)
            .unwrap_or(3);
        sink_proto(rec, head, now, failing_after)
    }

    fn validate_endpoint(&self, kind: i32, endpoint: &str) -> ApiResult<()> {
        let state = self.state()?;
        let kind = v1::AuditSinkKind::try_from(kind).unwrap_or(v1::AuditSinkKind::Unspecified);
        match kind {
            v1::AuditSinkKind::Unspecified => {
                Err(ApiError::invalid("missing_field", "kind is required"))
            }
            v1::AuditSinkKind::S3 => file_dir(&state.cfg.audit.file_roots, endpoint).map(|_| ()),
            _ => {
                if endpoint.starts_with("https://") || endpoint.starts_with("http://") {
                    Ok(())
                } else {
                    Err(ApiError::invalid(
                        "bad_endpoint",
                        "HTTPS, Splunk and Datadog sinks need an http(s) URL",
                    ))
                }
            }
        }
    }

    async fn check_secret(&self, cx: &Cx, org_id: &str, name: &str) -> ApiResult<()> {
        if name.is_empty() {
            return Ok(());
        }
        self.wiring
            .ci
            .get_secret(cx, &VaultScope::Org(org_id.to_string()), name)
            .await
            .map(|_| ())
            .map_err(|e| {
                if e.reason == ErrorReason::NotFound {
                    ApiError::invalid(
                        "unknown_secret",
                        format!("credential_secret {name:?} is not an org secret"),
                    )
                } else {
                    fe(e)
                }
            })
    }

    async fn create_sink(
        &self,
        caller: &ForgeCaller,
        r: v1::CreateAuditSinkRequest,
    ) -> ApiResult<v1::AuditSink> {
        let state = self.state()?;
        let (org, cx) = self
            .org_cx(caller, &r.org, &r.enterprise, OrgPerm::Manage)
            .await?;
        let name = r.name.trim().to_string();
        if name.is_empty() || name.len() > 100 {
            return Err(ApiError::invalid(
                "bad_name",
                "a sink name is 1 to 100 bytes",
            ));
        }
        self.validate_endpoint(r.kind, &r.endpoint)?;
        self.check_secret(&cx, &org.id, &r.credential_secret)
            .await?;
        let now = state.now_ms();
        let id = crate::util::new_ulid(now)?.to_text();
        let head = head_seq(&self.wiring, &org.id).await?;
        let rec = SinkRec {
            id: id.clone(),
            org_id: org.id.clone(),
            org: org.name.clone(),
            name: name.clone(),
            kind: r.kind,
            endpoint: r.endpoint.clone(),
            credential_secret: r.credential_secret.clone(),
            action_prefixes: r.action_prefixes.clone(),
            enabled: true,
            delivered_seq: if r.start_seq > 0 {
                r.start_seq - 1
            } else {
                head
            },
            created_ms: now,
            updated_ms: now,
            ..SinkRec::default()
        };
        let name_key = format!("{}/{name}", org.id);
        let ok = state
            .kv
            .apply(vec![
                crate::kv::KvOp::expect(NS_SINK_NAME, name_key.as_bytes().to_vec(), None),
                crate::kv::KvOp::put(NS_SINK_NAME, name_key.as_bytes().to_vec(), id.clone()),
                crate::kv::KvOp::put(
                    NS_SINK,
                    id.as_bytes().to_vec(),
                    serde_json::to_vec(&rec).unwrap_or_default(),
                ),
            ])
            .await?;
        if !ok {
            return Err(ApiError::new(
                ErrorReason::AlreadyExists,
                format!("org {} already has a sink named {name:?}", org.name),
            ));
        }
        self.audit(
            &cx,
            "audit_sink.create",
            &org.id,
            &id,
            &json!({ "name": name, "kind": r.kind, "endpoint": r.endpoint }).to_string(),
        )
        .await;
        Ok(self.sink_view(&rec).await)
    }

    async fn update_sink(
        &self,
        caller: &ForgeCaller,
        r: v1::UpdateAuditSinkRequest,
    ) -> ApiResult<v1::AuditSink> {
        let state = self.state()?;
        let (mut rec, cx) = self.sink(caller, &r.id, OrgPerm::Manage).await?;
        let old_name = rec.name.clone();
        if let Some(n) = &r.name {
            let n = n.trim();
            if n.is_empty() || n.len() > 100 {
                return Err(ApiError::invalid(
                    "bad_name",
                    "a sink name is 1 to 100 bytes",
                ));
            }
            rec.name = n.to_string();
        }
        if let Some(e) = &r.endpoint {
            self.validate_endpoint(rec.kind, e)?;
            rec.endpoint = e.clone();
        }
        if let Some(c) = &r.credential_secret {
            self.check_secret(&cx, &rec.org_id, c).await?;
            rec.credential_secret = c.clone();
        }
        if !r.action_prefixes.is_empty() {
            rec.action_prefixes = r.action_prefixes.clone();
        }
        if let Some(en) = r.enabled {
            if en && !rec.enabled {
                rec.failures = 0;
                rec.next_attempt_ms = 0;
            }
            rec.enabled = en;
        }
        rec.updated_ms = state.now_ms();
        let mut ops = vec![crate::kv::KvOp::put(
            NS_SINK,
            rec.id.as_bytes().to_vec(),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )];
        if rec.name != old_name {
            let new_key = format!("{}/{}", rec.org_id, rec.name);
            ops.insert(
                0,
                crate::kv::KvOp::expect(NS_SINK_NAME, new_key.as_bytes().to_vec(), None),
            );
            ops.push(crate::kv::KvOp::put(
                NS_SINK_NAME,
                new_key.as_bytes().to_vec(),
                rec.id.clone(),
            ));
            ops.push(crate::kv::KvOp::delete(
                NS_SINK_NAME,
                format!("{}/{old_name}", rec.org_id).into_bytes(),
            ));
        }
        if !state.kv.apply(ops).await? {
            return Err(ApiError::new(
                ErrorReason::AlreadyExists,
                format!("a sink named {:?} already exists", rec.name),
            ));
        }
        self.audit(&cx, "audit_sink.update", &rec.org_id, &rec.id, "")
            .await;
        Ok(self.sink_view(&rec).await)
    }

    async fn delete_sink(&self, caller: &ForgeCaller, id: &str) -> ApiResult<()> {
        let state = self.state()?;
        let (rec, cx) = self.sink(caller, id, OrgPerm::Manage).await?;
        state
            .kv
            .apply(vec![
                crate::kv::KvOp::delete(NS_SINK, rec.id.as_bytes().to_vec()),
                crate::kv::KvOp::delete(
                    NS_SINK_NAME,
                    format!("{}/{}", rec.org_id, rec.name).into_bytes(),
                ),
            ])
            .await?;
        self.audit(&cx, "audit_sink.delete", &rec.org_id, &rec.id, "")
            .await;
        Ok(())
    }

    async fn list_sinks(
        &self,
        caller: &ForgeCaller,
        r: v1::ListAuditSinksRequest,
    ) -> ApiResult<v1::ListAuditSinksResponse> {
        let state = self.state()?;
        let (org, _) = self
            .org_cx(caller, &r.org, &r.enterprise, OrgPerm::Manage)
            .await?;
        let mut all: Vec<SinkRec> = scan_json::<SinkRec>(state.kv.as_ref(), NS_SINK, "")
            .await?
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| s.org_id == org.id)
            .collect();
        all.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&all, &token, size)?;
        let mut sinks = Vec::new();
        for s in &page {
            sinks.push(self.sink_view(s).await);
        }
        Ok(v1::ListAuditSinksResponse {
            sinks,
            page: next_page(next),
        })
    }

    async fn test_sink(
        &self,
        caller: &ForgeCaller,
        id: &str,
    ) -> ApiResult<v1::TestAuditSinkResponse> {
        let state = self.state()?;
        let (rec, cx) = self.sink(caller, id, OrgPerm::Manage).await?;
        let entry = AuditEntry {
            id: crate::util::new_ulid(state.now_ms())?.to_text(),
            time: Some(timestamp_ms(state.now_ms())),
            action: "audit_sink.test".into(),
            org: rec.org_id.clone(),
            result: "ok".into(),
            ..AuditEntry::default()
        };
        let started = Instant::now();
        let out = deliver(&state, &self.wiring, &rec, &[entry]).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        self.audit(&cx, "audit_sink.test", &rec.org_id, &rec.id, "")
            .await;
        Ok(match out {
            Ok(code) => v1::TestAuditSinkResponse {
                delivered: true,
                status_code: code,
                error: String::new(),
                latency_ms,
            },
            Err((code, e)) => v1::TestAuditSinkResponse {
                delivered: false,
                status_code: code,
                error: e,
                latency_ms,
            },
        })
    }

    async fn export(
        &self,
        caller: &ForgeCaller,
        r: v1::ExportAuditLogRequest,
    ) -> ApiResult<v1::AuditExport> {
        let state = self.state()?;
        let (org, cx) = self
            .org_cx(caller, &r.org, &r.enterprise, OrgPerm::ReadAudit)
            .await?;
        let format = match v1::AuditExportFormat::try_from(r.format) {
            Ok(v1::AuditExportFormat::Csv) => v1::AuditExportFormat::Csv,
            Ok(v1::AuditExportFormat::Json) | Ok(v1::AuditExportFormat::Unspecified) => {
                v1::AuditExportFormat::Json
            }
            Err(_) => return Err(ApiError::invalid("bad_format", "unknown export format")),
        };
        let filter = r.filter.unwrap_or_default();
        // Compile the filter now so a bad phrase fails the call, not the job.
        self.wiring
            .forge
            .audit
            .query(&cx, &org_scope(&org.id), &filter, true, "", 1)
            .await
            .map_err(fe)?;
        let now = state.now_ms();
        let rec = ExportRec {
            id: crate::util::new_ulid(now)?.to_text(),
            org_id: org.id.clone(),
            requester: caller.cx.actor_id.clone(),
            format: format as i32,
            state: v1::AuditExportState::Pending as i32,
            created_ms: now,
            ..ExportRec::default()
        };
        put_json(state.kv.as_ref(), NS_EXPORT, &rec.id, &rec).await?;
        self.audit(&cx, "audit_log.export", &org.id, &rec.id, "")
            .await;
        let wiring = self.wiring.clone();
        let job = rec.clone();
        tokio::spawn(async move {
            let Some(state) = wiring.state() else { return };
            let done = run_export(&state, &wiring, job.clone(), filter).await;
            let fin = match done {
                Ok(r) => r,
                Err(e) => ExportRec {
                    state: v1::AuditExportState::Failed as i32,
                    error: e.detail.message.clone(),
                    ..job
                },
            };
            let _ = put_json(state.kv.as_ref(), NS_EXPORT, &fin.id, &fin).await;
        });
        export_proto(&state, &rec).await
    }

    async fn get_export(&self, caller: &ForgeCaller, id: &str) -> ApiResult<v1::AuditExport> {
        let state = self.state()?;
        if id.is_empty() {
            return Err(ApiError::invalid("missing_field", "id is required"));
        }
        let rec: ExportRec = get_json(state.kv.as_ref(), NS_EXPORT, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("audit export {id}")))?;
        self.org_cx(caller, &rec.org_id, "", OrgPerm::ReadAudit)
            .await?;
        export_proto(&state, &rec).await
    }
}

/// The last sequence of an org's log (0 when empty).
pub async fn head_seq(w: &ForgeWiring, org_id: &str) -> ApiResult<u64> {
    let (entries, _) = w
        .forge
        .audit
        .query(
            &Cx::system(),
            &org_scope(org_id),
            &AuditLogFilter::default(),
            false,
            "",
            1,
        )
        .await
        .map_err(fe)?;
    Ok(entries.first().map(|e| e.seq).unwrap_or(0))
}

fn sink_proto(rec: &SinkRec, head: u64, now: u64, failing_after: u32) -> v1::AuditSink {
    let lag = head.saturating_sub(rec.delivered_seq);
    let health = if !rec.enabled {
        v1::AuditSinkHealth::Paused
    } else if rec.failures >= failing_after.max(1) {
        v1::AuditSinkHealth::Failing
    } else if lag > 0
        && rec.last_delivered_ms > 0
        && now.saturating_sub(rec.last_delivered_ms) > 300_000
        || lag > 10_000
    {
        v1::AuditSinkHealth::Lagging
    } else {
        v1::AuditSinkHealth::Healthy
    };
    v1::AuditSink {
        id: rec.id.clone(),
        org: rec.org.clone(),
        enterprise: String::new(),
        name: rec.name.clone(),
        kind: rec.kind,
        endpoint: rec.endpoint.clone(),
        credential_secret: rec.credential_secret.clone(),
        action_prefixes: rec.action_prefixes.clone(),
        enabled: rec.enabled,
        health: health as i32,
        delivered_seq: rec.delivered_seq,
        lag_entries: lag,
        last_error: rec.last_error.clone(),
        last_delivered_at: (rec.last_delivered_ms > 0).then(|| timestamp_ms(rec.last_delivered_ms)),
        created_at: Some(timestamp_ms(rec.created_ms)),
        updated_at: Some(timestamp_ms(rec.updated_ms)),
    }
}

async fn export_proto(state: &AppState, rec: &ExportRec) -> ApiResult<v1::AuditExport> {
    let mut out = v1::AuditExport {
        id: rec.id.clone(),
        format: rec.format,
        state: rec.state,
        entry_count: rec.entry_count,
        first_seq: rec.first_seq,
        last_seq: rec.last_seq,
        head_hash: bee_core::hex::decode(&rec.head_hash).unwrap_or_default(),
        error: rec.error.clone(),
        created_at: Some(timestamp_ms(rec.created_ms)),
        ..v1::AuditExport::default()
    };
    if rec.state == v1::AuditExportState::Ready as i32 {
        let org = OrgId(bee_core::Ulid::parse(&rec.org_id).map_err(ApiError::from)?);
        let blob = bee_core::ObjectId::parse(&rec.blob).map_err(ApiError::from)?;
        match &state.url_issuer {
            Some(_) => {
                let ttl = Duration::from_secs(state.cfg.blobs.signed_url_ttl_secs);
                let url = state.issue_url(org, &blob, ttl, None, &[]).await?;
                out.signed_url = url.url;
                out.url_expires_at = Some(timestamp_ms(url.expires_at_ms));
            }
            None => {
                out.error = "signed URLs are disabled on this server (blobs.signed_urls)".into();
            }
        }
    }
    Ok(out)
}

/// One CSV field, quoted when needed.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// The CSV header of exports.
pub const CSV_HEADER: &str = "seq,id,time,actor,on_behalf_of,action,target,repo,org,request_id,result,policy_rule,prev_hash,hash";

/// One entry as a CSV row.
pub fn csv_row(e: &AuditEntry) -> String {
    let time = e
        .time
        .as_ref()
        .map(|t| (t.seconds.max(0) as u64) * 1000 + (t.nanos.max(0) as u64) / 1_000_000)
        .unwrap_or(0);
    let target = e
        .target
        .as_ref()
        .map(|t| format!("{}:{}", t.kind, t.id))
        .unwrap_or_default();
    let repo = match e.repo.as_ref().and_then(|r| r.repo.as_ref()) {
        Some(v1::repo_ref::Repo::Id(i)) => i.clone(),
        Some(v1::repo_ref::Repo::FullName(n)) => n.clone(),
        None => String::new(),
    };
    [
        e.seq.to_string(),
        e.id.clone(),
        time.to_string(),
        e.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
        e.on_behalf_of.clone(),
        e.action.clone(),
        target,
        repo,
        e.org.clone(),
        e.request_id.clone(),
        e.result.clone(),
        e.policy_rule.clone(),
        bee_core::hex::encode(&e.prev_hash),
        bee_core::hex::encode(&e.hash),
    ]
    .iter()
    .map(|f| csv_field(f))
    .collect::<Vec<_>>()
    .join(",")
}

async fn run_export(
    state: &AppState,
    w: &ForgeWiring,
    mut rec: ExportRec,
    filter: AuditLogFilter,
) -> ApiResult<ExportRec> {
    let scope = org_scope(&rec.org_id);
    let cx = Cx::system().with_timeout(Duration::from_secs(600));
    let csv = rec.format == v1::AuditExportFormat::Csv as i32;
    let mut body = String::new();
    if csv {
        body.push_str(CSV_HEADER);
        body.push('\n');
    }
    let mut token = String::new();
    loop {
        let (entries, next) = w
            .forge
            .audit
            .query(&cx, &scope, &filter, true, &token, 500)
            .await
            .map_err(fe)?;
        for e in &entries {
            if rec.first_seq == 0 {
                rec.first_seq = e.seq;
            }
            rec.last_seq = e.seq;
            rec.head_hash = bee_core::hex::encode(&e.hash);
            rec.entry_count += 1;
            if csv {
                body.push_str(&csv_row(e));
            } else {
                body.push_str(&serde_json::to_string(e).map_err(|err| {
                    ApiError::new(ErrorReason::Internal, format!("encode entry: {err}"))
                })?);
            }
            body.push('\n');
        }
        if next.is_empty() {
            break;
        }
        token = next;
    }
    let bytes = body.into_bytes();
    let id = bee_object::blob_id(&bytes);
    let org = OrgId(bee_core::Ulid::parse(&rec.org_id).map_err(ApiError::from)?);
    state
        .writer
        .upload(
            org,
            vec![(id, bytes)],
            Some(UploadRecord {
                repo: None,
                principal: rec.requester.clone(),
            }),
            state.now_ms(),
        )
        .await?;
    rec.blob = id.to_text();
    rec.state = v1::AuditExportState::Ready as i32;
    Ok(rec)
}

/// The directory of a `file://` endpoint, which must lie under one of `roots`.
fn file_dir(roots: &[String], endpoint: &str) -> ApiResult<PathBuf> {
    let Some(path) = endpoint.strip_prefix("file://") else {
        return Err(ApiError::invalid(
            "bad_endpoint",
            "S3 sinks on this server write to a `file:///dir` endpoint under audit.file_roots",
        ));
    };
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(ApiError::invalid(
            "bad_endpoint",
            "file endpoints are absolute paths without `..`",
        ));
    }
    if !roots.iter().any(|r| !r.is_empty() && p.starts_with(r)) {
        return Err(ApiError::invalid(
            "bad_endpoint",
            format!("{path} is not under a configured audit.file_roots entry"),
        ));
    }
    Ok(p.to_path_buf())
}

/// Delivers one batch; `Ok(status)` or `Err((status, error))`.
async fn deliver(
    state: &AppState,
    w: &ForgeWiring,
    rec: &SinkRec,
    entries: &[AuditEntry],
) -> Result<u32, (u32, String)> {
    let kind = v1::AuditSinkKind::try_from(rec.kind).unwrap_or(v1::AuditSinkKind::Unspecified);
    let json_of = |e: &AuditEntry| serde_json::to_value(e).unwrap_or(serde_json::Value::Null);
    if kind == v1::AuditSinkKind::S3 {
        let dir = file_dir(&state.cfg.audit.file_roots, &rec.endpoint)
            .map_err(|e| (0, e.detail.message.clone()))?;
        let first = entries.first().map(|e| e.seq).unwrap_or(0);
        let last = entries.last().map(|e| e.seq).unwrap_or(0);
        let mut body = String::new();
        for e in entries {
            body.push_str(&json_of(e).to_string());
            body.push('\n');
        }
        let name = format!("bee-audit-{}-{first:020}-{last:020}.jsonl", rec.org_id);
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".tmp-{name}"));
            std::fs::write(&tmp, body.as_bytes())?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, dir.join(&name))
        };
        return write()
            .map(|_| 0)
            .map_err(|e| (0, format!("file sink: {e}")));
    }
    let secret = if rec.credential_secret.is_empty() {
        String::new()
    } else {
        w.ci.secret_value(
            &Cx::system(),
            &VaultScope::Org(rec.org_id.clone()),
            &rec.credential_secret,
        )
        .await
        .map_err(|e| (0, format!("credential: {}", e.message)))?
    };
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("user-agent".to_string(), "bee-audit-sink/1".to_string());
    let body = match kind {
        v1::AuditSinkKind::Splunk => {
            headers.insert("content-type".into(), "application/json".into());
            if !secret.is_empty() {
                headers.insert("authorization".into(), format!("Splunk {secret}"));
            }
            entries
                .iter()
                .map(|e| json!({ "event": json_of(e), "sourcetype": "bee:audit" }).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
        v1::AuditSinkKind::Datadog => {
            headers.insert("content-type".into(), "application/json".into());
            if !secret.is_empty() {
                headers.insert("dd-api-key".into(), secret.clone());
            }
            serde_json::Value::Array(
                entries
                    .iter()
                    .map(|e| {
                        json!({
                            "ddsource": "bee",
                            "service": "beehive-audit",
                            "message": json_of(e).to_string(),
                        })
                    })
                    .collect(),
            )
            .to_string()
        }
        _ => {
            headers.insert("content-type".into(), "application/x-ndjson".into());
            if !secret.is_empty() {
                headers.insert("authorization".into(), format!("Bearer {secret}"));
            }
            let mut s = String::new();
            for e in entries {
                s.push_str(&json_of(e).to_string());
                s.push('\n');
            }
            s
        }
    };
    let sender = Sender::new(Duration::from_millis(
        state.cfg.audit.sink_timeout_ms.max(100),
    ));
    let out = sender
        .send(&rec.endpoint, &Prepared { body, headers })
        .await;
    if out.success() {
        Ok(u32::from(out.status))
    } else if out.error.is_empty() {
        Err((u32::from(out.status), format!("HTTP {}", out.status)))
    } else {
        Err((u32::from(out.status), out.error))
    }
}

/// One delivery pass over every enabled sink whose backoff has passed.
pub async fn deliver_all(state: &AppState, w: &ForgeWiring) -> ApiResult<()> {
    let sinks = scan_json::<SinkRec>(state.kv.as_ref(), NS_SINK, "").await?;
    for (_, sink) in sinks {
        if !sink.enabled || state.now_ms() < sink.next_attempt_ms {
            continue;
        }
        if let Err(e) = deliver_one(state, w, sink).await {
            logging::log(
                Level::Warn,
                "audit",
                "audit sink pass failed",
                &[("error", e.detail.message.into())],
            );
        }
    }
    Ok(())
}

async fn deliver_one(state: &AppState, w: &ForgeWiring, sink: SinkRec) -> ApiResult<()> {
    let batch = state.cfg.audit.sink_batch.clamp(1, 500);
    let token = if sink.delivered_seq == 0 {
        String::new()
    } else {
        sink.delivered_seq.to_string()
    };
    let (entries, _) = w
        .forge
        .audit
        .query(
            &Cx::system(),
            &org_scope(&sink.org_id),
            &AuditLogFilter::default(),
            true,
            &token,
            batch,
        )
        .await
        .map_err(fe)?;
    let Some(last) = entries.last().map(|e| e.seq) else {
        return Ok(());
    };
    let matching: Vec<AuditEntry> = entries
        .into_iter()
        .filter(|e| {
            sink.action_prefixes.is_empty()
                || sink.action_prefixes.iter().any(|p| e.action.starts_with(p))
        })
        .collect();
    let result = if matching.is_empty() {
        Ok(0)
    } else {
        deliver(state, w, &sink, &matching).await
    };
    // Only delivery fields change, compare-and-set against the row just read, so a
    // configuration change (UpdateAuditSink) that lands meanwhile is never overwritten.
    for _ in 0..8 {
        let Some(raw) = state.kv.get(NS_SINK, sink.id.as_bytes()).await? else {
            return Ok(());
        };
        let mut cur: SinkRec = serde_json::from_slice(&raw)
            .map_err(|e| ApiError::new(ErrorReason::Internal, format!("bad {NS_SINK} row: {e}")))?;
        let now = state.now_ms();
        match &result {
            Ok(_) => {
                cur.delivered_seq = cur.delivered_seq.max(last);
                cur.failures = 0;
                cur.last_error = String::new();
                cur.next_attempt_ms = 0;
                if !matching.is_empty() {
                    cur.last_delivered_ms = now;
                }
            }
            Err((_, e)) => {
                cur.failures = cur.failures.saturating_add(1);
                let base = state.cfg.audit.sink_retry_base_ms.max(1);
                let delay = base
                    .saturating_mul(1u64 << cur.failures.saturating_sub(1).min(20))
                    .min(state.cfg.audit.sink_retry_max_ms.max(base));
                cur.next_attempt_ms = now + delay;
                cur.last_error = e.clone();
                if cur.failures == state.cfg.audit.sink_failing_after.max(1) {
                    logging::log(
                        Level::Warn,
                        "audit",
                        "audit sink is failing",
                        &[("sink", cur.id.clone().into()), ("error", e.clone().into())],
                    );
                }
            }
        }
        let ops = vec![
            crate::kv::KvOp::expect(NS_SINK, cur.id.as_bytes().to_vec(), Some(raw)),
            crate::kv::KvOp::put(
                NS_SINK,
                cur.id.as_bytes().to_vec(),
                serde_json::to_vec(&cur).unwrap_or_default(),
            ),
        ];
        if state.kv.apply(ops).await? {
            return Ok(());
        }
    }
    Ok(())
}

/// The sink delivery worker.
pub fn spawn(state: Arc<AppState>, w: Arc<ForgeWiring>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let every = Duration::from_millis(state.cfg.audit.sink_poll_ms.max(10));
        loop {
            tokio::time::sleep(every).await;
            let _ = deliver_all(&state, &w).await;
        }
    })
}

macro_rules! forge_unary {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident| $body:expr) => {{
        let ($caller, rid) = $self.authn(&$req)?;
        let $inner = $req.into_inner();
        $body.await.map(Response::new).map_err(|e| status(&rid, e))
    }};
}

#[tonic::async_trait]
impl AuditService for AuditApi {
    type StreamAuditLogStream = <ForgeServices as AuditService>::StreamAuditLogStream;

    async fn query_audit_log(
        &self,
        req: Request<v1::QueryAuditLogRequest>,
    ) -> Result<Response<v1::QueryAuditLogResponse>, Status> {
        self.base.query_audit_log(req).await
    }

    async fn get_audit_entry(
        &self,
        req: Request<v1::GetAuditEntryRequest>,
    ) -> Result<Response<AuditEntry>, Status> {
        self.base.get_audit_entry(req).await
    }

    async fn stream_audit_log(
        &self,
        req: Request<v1::StreamAuditLogRequest>,
    ) -> Result<Response<Self::StreamAuditLogStream>, Status> {
        self.base.stream_audit_log(req).await
    }

    async fn verify_audit_chain(
        &self,
        req: Request<v1::VerifyAuditChainRequest>,
    ) -> Result<Response<v1::VerifyAuditChainResponse>, Status> {
        self.base.verify_audit_chain(req).await
    }

    async fn create_audit_sink(
        &self,
        req: Request<v1::CreateAuditSinkRequest>,
    ) -> Result<Response<v1::AuditSink>, Status> {
        forge_unary!(self, req, |caller, r| self.create_sink(&caller, r))
    }

    async fn get_audit_sink(
        &self,
        req: Request<v1::GetAuditSinkRequest>,
    ) -> Result<Response<v1::AuditSink>, Status> {
        forge_unary!(self, req, |caller, r| async {
            let (rec, _) = self.sink(&caller, &r.id, OrgPerm::Manage).await?;
            Ok(self.sink_view(&rec).await)
        })
    }

    async fn list_audit_sinks(
        &self,
        req: Request<v1::ListAuditSinksRequest>,
    ) -> Result<Response<v1::ListAuditSinksResponse>, Status> {
        forge_unary!(self, req, |caller, r| self.list_sinks(&caller, r))
    }

    async fn update_audit_sink(
        &self,
        req: Request<v1::UpdateAuditSinkRequest>,
    ) -> Result<Response<v1::AuditSink>, Status> {
        forge_unary!(self, req, |caller, r| self.update_sink(&caller, r))
    }

    async fn delete_audit_sink(
        &self,
        req: Request<v1::DeleteAuditSinkRequest>,
    ) -> Result<Response<v1::DeleteAuditSinkResponse>, Status> {
        forge_unary!(self, req, |caller, r| async {
            self.delete_sink(&caller, &r.id).await?;
            Ok(v1::DeleteAuditSinkResponse {})
        })
    }

    async fn test_audit_sink(
        &self,
        req: Request<v1::TestAuditSinkRequest>,
    ) -> Result<Response<v1::TestAuditSinkResponse>, Status> {
        forge_unary!(self, req, |caller, r| self.test_sink(&caller, &r.id))
    }

    async fn export_audit_log(
        &self,
        req: Request<v1::ExportAuditLogRequest>,
    ) -> Result<Response<v1::AuditExport>, Status> {
        forge_unary!(self, req, |caller, r| self.export(&caller, r))
    }

    async fn get_audit_export(
        &self,
        req: Request<v1::GetAuditExportRequest>,
    ) -> Result<Response<v1::AuditExport>, Status> {
        forge_unary!(self, req, |caller, r| self.get_export(&caller, &r.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_and_file_roots_are_enforced() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"x\""), "\"say \"\"x\"\"\"");
        assert_eq!(csv_field("plain"), "plain");
        let roots = vec!["/var/audit".to_string()];
        assert!(file_dir(&roots, "file:///var/audit/org1").is_ok());
        assert!(file_dir(&roots, "file:///etc").is_err());
        assert!(file_dir(&roots, "file:///var/audit/../etc").is_err());
        assert!(file_dir(&roots, "s3://bucket").is_err());
        assert!(file_dir(&[], "file:///var/audit").is_err());
    }

    #[test]
    fn health_follows_failures_and_pause() {
        let mut s = SinkRec {
            enabled: true,
            delivered_seq: 5,
            ..SinkRec::default()
        };
        assert_eq!(
            sink_proto(&s, 5, 0, 3).health,
            v1::AuditSinkHealth::Healthy as i32
        );
        s.failures = 3;
        assert_eq!(
            sink_proto(&s, 9, 0, 3).health,
            v1::AuditSinkHealth::Failing as i32
        );
        assert_eq!(sink_proto(&s, 9, 0, 3).lag_entries, 4);
        s.enabled = false;
        assert_eq!(
            sink_proto(&s, 9, 0, 3).health,
            v1::AuditSinkHealth::Paused as i32
        );
    }
}
