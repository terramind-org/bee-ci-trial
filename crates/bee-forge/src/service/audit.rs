//! `AuditService`: query, get, stream and verify an org's audit log (owners, admins and
//! security managers). Enterprise logs, SIEM sinks and exports belong to the `enterprise`
//! module.

use bee_proto::v1::audit_service_server::AuditService;
use bee_proto::v1::{
    AuditEntry, AuditExport, AuditSink, CreateAuditSinkRequest, DeleteAuditSinkRequest,
    DeleteAuditSinkResponse, ExportAuditLogRequest, GetAuditEntryRequest, GetAuditExportRequest,
    GetAuditSinkRequest, ListAuditSinksRequest, ListAuditSinksResponse, QueryAuditLogRequest,
    QueryAuditLogResponse, StreamAuditLogRequest, TestAuditSinkRequest, TestAuditSinkResponse,
    UpdateAuditSinkRequest, VerifyAuditChainRequest, VerifyAuditChainResponse,
};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::events::{REAUTHORIZE_EVERY, reauthorize_every};
use super::{ForgeServices, ResponseStream, next, page, reply, unserved};
use crate::audit::{EntryKey, org_scope};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};

impl ForgeServices {
    async fn audit_scope(
        &self,
        caller: &Caller,
        org: &str,
        enterprise: &str,
    ) -> Result<(String, Cx)> {
        if !enterprise.is_empty() {
            return Err(unserved("AuditService (enterprise logs)", "enterprise"));
        }
        if org.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        let org = self.forge.orgs.get_org(&caller.cx, org).await?;
        let cx = self
            .forge
            .authz
            .org(caller, &org.id, OrgPerm::ReadAudit)
            .await?;
        Ok((org_scope(&org.id), cx))
    }
}

#[tonic::async_trait]
impl AuditService for ForgeServices {
    type StreamAuditLogStream = ResponseStream<AuditEntry>;

    async fn query_audit_log(
        &self,
        req: Request<QueryAuditLogRequest>,
    ) -> Result<Response<QueryAuditLogResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.audit_scope(&caller, &r.org, &r.enterprise).await?;
            let (token, size) = page(&r.page);
            let filter = r.filter.unwrap_or_default();
            let (entries, next_token) = self
                .forge
                .audit
                .query(&cx, &scope, &filter, r.oldest_first, &token, size)
                .await?;
            Ok(QueryAuditLogResponse {
                entries,
                page: next(next_token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_audit_entry(
        &self,
        req: Request<GetAuditEntryRequest>,
    ) -> Result<Response<AuditEntry>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.audit_scope(&caller, &r.org, &r.enterprise).await?;
            let key = r
                .key
                .as_ref()
                .map(EntryKey::from)
                .ok_or_else(|| ForgeError::missing("key"))?;
            self.forge.audit.get(&cx, &scope, &key).await
        }
        .await;
        reply(&caller, out)
    }

    async fn stream_audit_log(
        &self,
        req: Request<StreamAuditLogRequest>,
    ) -> Result<Response<Self::StreamAuditLogStream>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.audit_scope(&caller, &r.org, &r.enterprise).await?;
            let filter = r.filter.unwrap_or_default();
            let rx = self
                .forge
                .audit
                .stream(&cx, &scope, r.after_seq, &filter)
                .await?;
            let (tx, checked_rx) = tokio::sync::mpsc::channel(64);
            let service = self.clone();
            let caller = caller.clone();
            let org = r.org.clone();
            let enterprise = r.enterprise.clone();
            tokio::spawn(async move {
                let mut upstream = rx;
                let mut checked = tokio::time::Instant::now();
                loop {
                    tokio::select! {
                        item = upstream.recv() => match item {
                            Some(item) => {
                                if let Err(e) = reauthorize_every(&mut checked, || async {
                                    let fresh = service.fresh(&caller)?;
                                    service.audit_scope(&fresh, &org, &enterprise).await.map(|_| ())
                                }).await {
                                    let _ = tx.send(Err(e)).await;
                                    return;
                                }
                                if tx.send(item).await.is_err() { return; }
                            }
                            None => return,
                        },
                        _ = tokio::time::sleep(REAUTHORIZE_EVERY) => {
                            let result = async {
                                let fresh = service.fresh(&caller)?;
                                service.audit_scope(&fresh, &org, &enterprise).await
                            }.await;
                            if let Err(e) = result {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                            checked = tokio::time::Instant::now();
                        }
                    }
                }
            });
            let rid = cx.request_id.clone();
            let s = ReceiverStream::new(checked_rx).map(move |i| i.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(s) as ResponseStream<AuditEntry>)
        }
        .await;
        reply(&caller, out)
    }

    async fn verify_audit_chain(
        &self,
        req: Request<VerifyAuditChainRequest>,
    ) -> Result<Response<VerifyAuditChainResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.audit_scope(&caller, &r.org, &r.enterprise).await?;
            self.forge
                .audit
                .verify(&cx, &scope, r.from_seq, r.to_seq, &r.expected_head_hash)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_audit_sink(
        &self,
        req: Request<CreateAuditSinkRequest>,
    ) -> Result<Response<AuditSink>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.CreateAuditSink",
                "enterprise (SIEM sinks)",
            )),
        )
    }

    async fn get_audit_sink(
        &self,
        req: Request<GetAuditSinkRequest>,
    ) -> Result<Response<AuditSink>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.GetAuditSink",
                "enterprise (SIEM sinks)",
            )),
        )
    }

    async fn list_audit_sinks(
        &self,
        req: Request<ListAuditSinksRequest>,
    ) -> Result<Response<ListAuditSinksResponse>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.ListAuditSinks",
                "enterprise (SIEM sinks)",
            )),
        )
    }

    async fn update_audit_sink(
        &self,
        req: Request<UpdateAuditSinkRequest>,
    ) -> Result<Response<AuditSink>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.UpdateAuditSink",
                "enterprise (SIEM sinks)",
            )),
        )
    }

    async fn delete_audit_sink(
        &self,
        req: Request<DeleteAuditSinkRequest>,
    ) -> Result<Response<DeleteAuditSinkResponse>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.DeleteAuditSink",
                "enterprise (SIEM sinks)",
            )),
        )
    }

    async fn test_audit_sink(
        &self,
        req: Request<TestAuditSinkRequest>,
    ) -> Result<Response<TestAuditSinkResponse>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.TestAuditSink",
                "enterprise (SIEM sinks)",
            )),
        )
    }

    async fn export_audit_log(
        &self,
        req: Request<ExportAuditLogRequest>,
    ) -> Result<Response<AuditExport>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.ExportAuditLog",
                "enterprise (exports to the blob store)",
            )),
        )
    }

    async fn get_audit_export(
        &self,
        req: Request<GetAuditExportRequest>,
    ) -> Result<Response<AuditExport>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "AuditService.GetAuditExport",
                "enterprise (exports to the blob store)",
            )),
        )
    }
}
