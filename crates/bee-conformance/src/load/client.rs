//! A small typed gRPC client for the scale tests and the load harness.
//!
//! Each method is one RPC (or a short fixed sequence, like upload-then-create) against a
//! running server, returning domain types from `bee-core` and an [`RpcError`] that carries
//! the SPEC §12.2 [`ErrorDetail`] decoded from the status.

use std::time::Duration;

use bee_core::{ChangeId, ObjectId};
use bee_proto::errors::detail_from_status;
use bee_proto::v1::{
    self, ErrorDetail, object_service_client::ObjectServiceClient,
    query_service_client::QueryServiceClient, ref_service_client::RefServiceClient,
    write_service_client::WriteServiceClient,
};
use tonic::transport::{Channel, Endpoint};

/// A failed RPC: the status and its decoded error detail.
#[derive(Clone, Debug)]
pub struct RpcError {
    /// The gRPC status.
    pub status: Box<tonic::Status>,
    /// The SPEC §12.2 detail, when the server attached one.
    pub detail: Option<Box<ErrorDetail>>,
}

impl RpcError {
    /// The reason name (`MERGE_CONFLICT`, …), or the gRPC code name without a detail.
    pub fn reason(&self) -> String {
        match &self.detail {
            Some(d) => d.reason.clone(),
            None => format!("{:?}", self.status.code()),
        }
    }

    /// Whether the server marked the failure retryable (transport failures count as
    /// retryable, since the request may never have arrived).
    pub fn retryable(&self) -> bool {
        self.detail.as_ref().is_none_or(|d| d.retryable)
    }

    /// The `current_version` of a `VERSION_MISMATCH` payload.
    pub fn current_version(&self) -> Option<u64> {
        match self.detail.as_ref()?.payload.as_ref()? {
            v1::error_detail::Payload::VersionMismatch(v) => Some(v.current_version),
            _ => None,
        }
    }
}

impl From<tonic::Status> for RpcError {
    fn from(status: tonic::Status) -> RpcError {
        let detail = detail_from_status(&status).map(Box::new);
        RpcError {
            status: Box::new(status),
            detail,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason(), self.status.message())
    }
}

impl std::error::Error for RpcError {}

/// Result of an RPC.
pub type RpcResult<T> = Result<T, RpcError>;

/// One member of a land answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    /// The change.
    pub change_id: ChangeId,
    /// The changeset asked to land.
    pub source: ObjectId,
    /// The changeset on the ref.
    pub result: ObjectId,
    /// Already landed from the same source.
    pub skipped: bool,
}

/// A land answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
    /// The ref version naming this land.
    pub version: u64,
    /// The ref target after it.
    pub target: ObjectId,
    /// Members in stack order.
    pub members: Vec<Member>,
    /// Repo event sequence of the land (0 when nothing new was published).
    pub event_seq: u64,
}

/// One ref-log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// The version written.
    pub version: u64,
    /// Target before.
    pub old: Option<ObjectId>,
    /// Target after.
    pub new: Option<ObjectId>,
    /// Who wrote it.
    pub principal: String,
    /// Changes landed by it.
    pub landed: Vec<Member>,
}

/// The proto form of an object id.
pub fn pid(id: &ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// An object id from its proto form.
pub fn oid(id: Option<&v1::ObjectId>) -> Option<ObjectId> {
    ObjectId::from_bytes(&id?.raw).ok()
}

/// A ChangeId built from a number (distinct numbers give distinct ids).
pub fn change_id(n: u64) -> ChangeId {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b[8] = 0x5c;
    b[15] = 1;
    ChangeId::new(b)
}

fn pchange(c: &ChangeId) -> v1::ChangeId {
    v1::ChangeId {
        raw: c.as_bytes().to_vec(),
    }
}

fn member(m: &v1::LandedChange) -> Option<Member> {
    Some(Member {
        change_id: ChangeId::from_bytes(&m.change_id.as_ref()?.raw).ok()?,
        source: oid(m.source.as_ref())?,
        result: oid(m.result.as_ref())?,
        skipped: m.skipped,
    })
}

fn bad(what: &str) -> RpcError {
    RpcError::from(tonic::Status::internal(format!(
        "malformed response: {what}"
    )))
}

/// Opens a channel to `url` (HTTP/2, loopback friendly).
pub async fn connect(url: &str) -> Result<Channel, tonic::transport::Error> {
    Endpoint::from_shared(url.to_string())?
        .tcp_nodelay(true)
        .connect()
        .await
}

/// A file to put: path and content.
pub type File = (String, Vec<u8>);

/// A client for one repo with one token.
#[derive(Clone, Debug)]
pub struct Client {
    ch: Channel,
    token: String,
    repo: String,
    timeout: Duration,
}

impl Client {
    /// A client over `ch` acting with `token` on the repo `org/name`.
    pub fn new(ch: Channel, token: impl Into<String>, repo: impl Into<String>) -> Client {
        Client {
            ch,
            token: token.into(),
            repo: repo.into(),
            timeout: Duration::from_secs(120),
        }
    }

    /// The same client with another token.
    pub fn with_token(&self, token: impl Into<String>) -> Client {
        Client {
            token: token.into(),
            ..self.clone()
        }
    }

    /// The same client with a per-request deadline (sent as `grpc-timeout`).
    pub fn with_timeout(&self, timeout: Duration) -> Client {
        Client {
            timeout,
            ..self.clone()
        }
    }

    /// The channel.
    pub fn channel(&self) -> &Channel {
        &self.ch
    }

    /// The token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The repo reference sent with every call.
    pub fn repo(&self) -> Option<v1::RepoRef> {
        Some(v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::FullName(self.repo.clone())),
        })
    }

    /// A request carrying the bearer token and the deadline.
    pub fn req<T>(&self, m: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(m);
        if let Ok(v) = format!("Bearer {}", self.token).parse() {
            r.metadata_mut().insert("authorization", v);
        }
        r.set_timeout(self.timeout);
        r
    }

    fn write(&self) -> WriteServiceClient<Channel> {
        WriteServiceClient::new(self.ch.clone())
    }

    fn query(&self) -> QueryServiceClient<Channel> {
        QueryServiceClient::new(self.ch.clone())
    }

    fn refs(&self) -> RefServiceClient<Channel> {
        RefServiceClient::new(self.ch.clone())
    }

    /// Uploads blobs (one stream).
    pub async fn upload(&self, blobs: &[Vec<u8>]) -> RpcResult<()> {
        if blobs.is_empty() {
            return Ok(());
        }
        let items: Vec<v1::UploadItem> = blobs
            .iter()
            .enumerate()
            .map(|(i, b)| v1::UploadItem {
                repo: if i == 0 { self.repo() } else { None },
                id: Some(pid(&bee_object::blob_id(b))),
                offset: 0,
                data: b.clone(),
                last: true,
            })
            .collect();
        self.write()
            .upload_objects(self.req(futures_util::stream::iter(items)))
            .await?;
        Ok(())
    }

    /// Creates a changeset on `parent` putting already-uploaded blobs at paths.
    pub async fn create(
        &self,
        parent: &ObjectId,
        puts: &[(String, ObjectId, u64)],
        change: ChangeId,
        message: &str,
    ) -> RpcResult<ObjectId> {
        let delta = puts
            .iter()
            .map(|(path, blob, size)| v1::DeltaEntry {
                path: path.as_bytes().to_vec(),
                op: Some(v1::delta_entry::Op::Put(v1::Put {
                    mode: 1,
                    blob: Some(pid(blob)),
                    size: *size,
                })),
            })
            .collect();
        let cs = self
            .write()
            .create_changeset(self.req(v1::CreateChangesetRequest {
                repo: self.repo(),
                parents: vec![pid(parent)],
                delta,
                change_id: Some(pchange(&change)),
                message: message.to_string(),
                author_time: Some(v1::Signature {
                    name: "agent".into(),
                    time: Some(pbjson_types::Timestamp {
                        seconds: 1_790_000_000,
                        nanos: 0,
                    }),
                    tz_offset_minutes: 0,
                }),
                ..Default::default()
            }))
            .await?
            .into_inner()
            .changeset;
        oid(cs.as_ref().and_then(|c| c.id.as_ref())).ok_or_else(|| bad("changeset id"))
    }

    /// Uploads the files' contents and creates one changeset putting them on `parent`.
    pub async fn commit(
        &self,
        parent: &ObjectId,
        files: &[File],
        change: ChangeId,
        message: &str,
    ) -> RpcResult<ObjectId> {
        let blobs: Vec<Vec<u8>> = files.iter().map(|(_, d)| d.clone()).collect();
        self.upload(&blobs).await?;
        let puts: Vec<(String, ObjectId, u64)> = files
            .iter()
            .map(|(p, d)| (p.clone(), bee_object::blob_id(d), d.len() as u64))
            .collect();
        self.create(parent, &puts, change, message).await
    }

    /// Lands the stack ending at `head` on `ref_name`.
    pub async fn land(&self, ref_name: &str, head: &ObjectId) -> RpcResult<Landed> {
        self.land_with(ref_name, head, "").await
    }

    /// Lands on behalf of a change request (`change_request_id` may be empty).
    pub async fn land_with(
        &self,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> RpcResult<Landed> {
        let r = self
            .write()
            .land(self.req(v1::LandRequest {
                repo: self.repo(),
                r#ref: ref_name.to_string(),
                head: Some(pid(head)),
                change_request_id: change_request_id.to_string(),
                ..Default::default()
            }))
            .await?
            .into_inner();
        let rf = r.r#ref.ok_or_else(|| bad("land ref"))?;
        Ok(Landed {
            version: rf.version,
            target: oid(rf.target.as_ref()).ok_or_else(|| bad("land target"))?,
            members: r.landed.iter().filter_map(member).collect(),
            event_seq: r.event_seq,
        })
    }

    /// Resolves a ref strongly: `(target, version)`, or `None` when it does not exist. A stale
    /// answer (served during a metadata outage, SPEC §15.1) is a retryable `UNAVAILABLE`
    /// here; [`Client::resolve_allow_stale`] accepts it.
    pub async fn resolve(&self, name: &str) -> RpcResult<Option<(ObjectId, u64)>> {
        match self.resolve_allow_stale(name).await? {
            Some((_, _, true)) => {
                let why = "stale ref read (metadata primary unavailable)";
                Err(RpcError {
                    status: Box::new(tonic::Status::unavailable(why)),
                    detail: Some(Box::new(bee_proto::errors::error_detail(
                        v1::ErrorReason::Unavailable,
                        why,
                    ))),
                })
            }
            other => Ok(other.map(|(t, v, _)| (t, v))),
        }
    }

    /// Resolves a ref, accepting a stale answer: `(target, version, stale)`, or `None` when
    /// it does not exist.
    pub async fn resolve_allow_stale(
        &self,
        name: &str,
    ) -> RpcResult<Option<(ObjectId, u64, bool)>> {
        let r = ObjectServiceClient::new(self.ch.clone())
            .resolve_ref(self.req(v1::ResolveRefRequest {
                repo: self.repo(),
                name: name.to_string(),
                ..Default::default()
            }))
            .await;
        match r {
            Ok(r) => {
                let r = r.into_inner();
                let stale = r.stale;
                Ok(r.r#ref
                    .and_then(|rf| oid(rf.target.as_ref()).map(|t| (t, rf.version, stale))))
            }
            Err(s) => {
                let e = RpcError::from(s);
                if e.reason() == "NOT_FOUND" {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Creates a ref (fails if it exists); returns its version.
    pub async fn create_ref(&self, name: &str, target: &ObjectId) -> RpcResult<u64> {
        let r = self
            .refs()
            .create_ref(self.req(v1::CreateRefRequest {
                repo: self.repo(),
                name: name.to_string(),
                target: Some(pid(target)),
                reason: "conformance".into(),
            }))
            .await?
            .into_inner();
        Ok(r.version)
    }

    /// Compare-and-swap of a non-protected ref from `expected` (non-zero) to `target`,
    /// fast-forward or not; returns the new version.
    pub async fn update_ref(&self, name: &str, target: &ObjectId, expected: u64) -> RpcResult<u64> {
        let r = self
            .refs()
            .update_ref(self.req(v1::UpdateRefRequest {
                repo: self.repo(),
                name: name.to_string(),
                target: Some(pid(target)),
                expected_version: expected,
                allow_non_fast_forward: true,
                reason: "conformance".into(),
            }))
            .await?
            .into_inner();
        Ok(r.version)
    }

    /// The whole ref log of a ref, oldest first.
    pub async fn ref_log(&self, name: &str) -> RpcResult<Vec<LogEntry>> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let r = self
                .refs()
                .get_ref_log(self.req(v1::GetRefLogRequest {
                    repo: self.repo(),
                    name: name.to_string(),
                    from_version: 1,
                    page: Some(v1::PageRequest {
                        page_size: 1000,
                        page_token: token.clone(),
                    }),
                }))
                .await?
                .into_inner();
            for e in &r.entries {
                out.push(LogEntry {
                    version: e.version,
                    old: oid(e.old.as_ref()),
                    new: oid(e.new.as_ref()),
                    principal: e.principal.clone(),
                    landed: e.landed.iter().filter_map(member).collect(),
                });
            }
            token = r.page.map(|p| p.next_page_token).unwrap_or_default();
            if token.is_empty() || r.entries.is_empty() {
                return Ok(out);
            }
        }
    }

    /// Parents of changesets, in request order.
    pub async fn parents(&self, ids: &[ObjectId]) -> RpcResult<Vec<(ObjectId, Vec<ObjectId>)>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(500) {
            let r = self
                .query()
                .get_changesets(self.req(v1::GetChangesetsRequest {
                    repo: self.repo(),
                    ids: chunk.iter().map(pid).collect(),
                    include_meta: false,
                }))
                .await?
                .into_inner();
            for c in r.changesets {
                let id = oid(c.id.as_ref()).ok_or_else(|| bad("changeset id"))?;
                let parents = c.parents.iter().filter_map(|p| oid(Some(p))).collect();
                out.push((id, parents));
            }
        }
        Ok(out)
    }

    /// One page of log from a revision; returns the number of entries.
    pub async fn log(&self, rev: &str, page_size: u32) -> RpcResult<usize> {
        self.log_path(rev, "", page_size).await
    }

    /// One page of log from a revision restricted to a path (empty = every path); returns
    /// the number of entries.
    pub async fn log_path(&self, rev: &str, path: &str, page_size: u32) -> RpcResult<usize> {
        let r = self
            .query()
            .log(self.req(v1::LogRequest {
                repo: self.repo(),
                from: Some(v1::Revision { expr: rev.into() }),
                path: path.as_bytes().to_vec(),
                include_meta: true,
                page: Some(v1::PageRequest {
                    page_size,
                    page_token: String::new(),
                }),
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(r.changesets.len())
    }

    /// Blame of a file at a revision; returns the number of ranges.
    pub async fn blame(&self, rev: &str, path: &str) -> RpcResult<usize> {
        let r = self
            .query()
            .blame(self.req(v1::BlameRequest {
                repo: self.repo(),
                at: Some(v1::Revision { expr: rev.into() }),
                path: path.as_bytes().to_vec(),
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(r.ranges.len())
    }

    /// Diff between two revisions with hunks, classified (SPEC §11.3) when `classify`;
    /// returns the items as (path, diff class).
    pub async fn diff(
        &self,
        from: &str,
        to: &str,
        classify: bool,
    ) -> RpcResult<Vec<(String, String)>> {
        let mut s = self
            .query()
            .diff(self.req(v1::DiffRequest {
                repo: self.repo(),
                from: Some(v1::Revision { expr: from.into() }),
                to: Some(v1::Revision { expr: to.into() }),
                include_hunks: true,
                classify,
                ..Default::default()
            }))
            .await?
            .into_inner();
        let mut out = Vec::new();
        while let Some(item) = s.message().await? {
            out.push((
                String::from_utf8_lossy(&item.path).into_owned(),
                item.diff_class,
            ));
        }
        Ok(out)
    }

    /// Lists a tree recursively under `prefix` at a revision: `(path, blob id, size)` of
    /// every file.
    pub async fn list_tree(
        &self,
        rev: &str,
        prefix: &str,
    ) -> RpcResult<Vec<(String, ObjectId, u64)>> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let r = self
                .query()
                .list_tree(self.req(v1::ListTreeRequest {
                    repo: self.repo(),
                    at: Some(v1::Revision { expr: rev.into() }),
                    path: prefix.as_bytes().to_vec(),
                    recursive: true,
                    page: Some(v1::PageRequest {
                        page_size: 1000,
                        page_token: token.clone(),
                    }),
                }))
                .await?
                .into_inner();
            for e in &r.entries {
                if let Some(id) = oid(e.id.as_ref())
                    && id.kind() == bee_core::ObjectKind::Blob
                {
                    out.push((String::from_utf8_lossy(&e.path).into_owned(), id, e.size));
                }
            }
            token = r.page.map(|p| p.next_page_token).unwrap_or_default();
            if token.is_empty() || r.entries.is_empty() {
                return Ok(out);
            }
        }
    }

    /// Fetches objects' bytes (streamed); returns the total bytes received.
    pub async fn get_objects(&self, ids: &[ObjectId]) -> RpcResult<u64> {
        let mut total = 0u64;
        for chunk in ids.chunks(1000) {
            let mut s = ObjectServiceClient::new(self.ch.clone())
                .get_objects(self.req(v1::GetObjectsRequest {
                    repo: self.repo(),
                    ids: chunk.iter().map(pid).collect(),
                    ..Default::default()
                }))
                .await?
                .into_inner();
            while let Some(c) = s.message().await? {
                total += c.data.len() as u64;
            }
        }
        Ok(total)
    }

    fn auth(&self) -> v1::auth_service_client::AuthServiceClient<Channel> {
        v1::auth_service_client::AuthServiceClient::new(self.ch.clone())
    }

    /// Registers an agent principal owned by the caller; returns its id.
    pub async fn create_agent(&self, display: &str) -> RpcResult<String> {
        let p = self
            .auth()
            .create_agent(self.req(v1::CreateAgentRequest {
                display: display.into(),
                runtime: "conformance".into(),
                purpose: "scale simulation".into(),
                org: String::new(),
            }))
            .await?
            .into_inner();
        Ok(p.id)
    }

    /// Mints a token for `principal_id` (attenuating the caller's) with a budget of at most
    /// `max_changesets` changesets and `max_landed` lands (0 = unlimited) and the given
    /// write paths (empty = inherit).
    pub async fn mint_token(
        &self,
        principal_id: &str,
        write_paths: &[&str],
        max_changesets: u64,
        max_landed: u64,
    ) -> RpcResult<String> {
        let r = self
            .auth()
            .mint_token(self.req(v1::MintTokenRequest {
                principal_id: principal_id.into(),
                scope: Some(v1::TokenScope {
                    write_paths: write_paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
                    budget: Some(v1::Budget {
                        max_changesets,
                        max_landed,
                        max_upload_bytes: 0,
                    }),
                    ..Default::default()
                }),
            }))
            .await?
            .into_inner();
        Ok(r.token)
    }

    /// Makes a principal a member of an org (the forge serves repos to members).
    pub async fn add_member(&self, org: &str, principal_id: &str) -> RpcResult<()> {
        v1::org_service_client::OrgServiceClient::new(self.ch.clone())
            .set_member(self.req(v1::SetMemberRequest {
                org: org.into(),
                principal_id: principal_id.into(),
                role: v1::Role::Member as i32,
            }))
            .await?;
        Ok(())
    }

    /// Gives a principal the write role on this client's repo (collaborator).
    pub async fn add_writer(&self, principal_id: &str) -> RpcResult<()> {
        v1::repo_service_client::RepoServiceClient::new(self.ch.clone())
            .add_collaborator(self.req(v1::AddCollaboratorRequest {
                repo: self.repo(),
                principal_id: principal_id.into(),
                role: v1::RepoRole::Write as i32,
                ..Default::default()
            }))
            .await?;
        Ok(())
    }

    /// `WhoAmI`: the principal id and the delegation chain back to its root.
    pub async fn who_am_i(&self) -> RpcResult<(String, Vec<String>)> {
        let r = self
            .auth()
            .who_am_i(self.req(v1::WhoAmIRequest::default()))
            .await?
            .into_inner();
        Ok((
            r.principal.map(|p| p.id).unwrap_or_default(),
            r.on_behalf_of,
        ))
    }

    fn changes(&self) -> v1::change_service_client::ChangeServiceClient<Channel> {
        v1::change_service_client::ChangeServiceClient::new(self.ch.clone())
    }

    /// Opens a change request for `head` against `target_ref`; returns its id.
    pub async fn open_change(
        &self,
        target_ref: &str,
        head: &ObjectId,
        title: &str,
    ) -> RpcResult<String> {
        let cr = self
            .changes()
            .open_change(self.req(v1::OpenChangeRequest {
                repo: self.repo(),
                target_ref: target_ref.into(),
                head: Some(pid(head)),
                title: title.into(),
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(cr.id)
    }

    /// Approves revision `revision` of a change request.
    pub async fn approve(&self, change_request_id: &str, revision: u32) -> RpcResult<()> {
        self.changes()
            .submit_review(self.req(v1::SubmitReviewRequest {
                repo: self.repo(),
                change_request_id: change_request_id.into(),
                revision,
                verdict: v1::Verdict::Approve as i32,
                body: "approved by the simulation".into(),
                comments: Vec::new(),
            }))
            .await?;
        Ok(())
    }

    /// Lands a change request; returns the land answer.
    pub async fn land_change(&self, id: &str) -> RpcResult<Landed> {
        let r = self
            .changes()
            .land_change(self.req(v1::LandChangeRequest {
                repo: self.repo(),
                id: id.into(),
                ..Default::default()
            }))
            .await?
            .into_inner();
        let land = r.land.ok_or_else(|| bad("land"))?;
        let rf = land.r#ref.ok_or_else(|| bad("land ref"))?;
        Ok(Landed {
            version: rf.version,
            target: oid(rf.target.as_ref()).ok_or_else(|| bad("land target"))?,
            members: land.landed.iter().filter_map(member).collect(),
            event_seq: land.event_seq,
        })
    }

    /// Change requests of the repo (first page of up to 1000), as `(id, state)`.
    pub async fn list_changes(&self) -> RpcResult<Vec<(String, i32)>> {
        let r = self
            .changes()
            .list_changes(self.req(v1::ListChangesRequest {
                repo: self.repo(),
                page: Some(v1::PageRequest {
                    page_size: 1000,
                    page_token: String::new(),
                }),
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(r.changes.into_iter().map(|c| (c.id, c.state)).collect())
    }

    fn leases(&self) -> v1::lease_service_client::LeaseServiceClient<Channel> {
        v1::lease_service_client::LeaseServiceClient::new(self.ch.clone())
    }

    /// Takes an exclusive lease on `paths` of `ref_name`; returns the lease id.
    pub async fn acquire_lease(&self, ref_name: &str, paths: &[&str]) -> RpcResult<String> {
        let r = self
            .leases()
            .acquire_lease(self.req(v1::AcquireLeaseRequest {
                repo: self.repo(),
                r#ref: ref_name.into(),
                paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
                mode: v1::LeaseMode::Exclusive as i32,
                ttl_seconds: 600,
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(r.lease.map(|l| l.id).unwrap_or_default())
    }

    /// Releases a lease.
    pub async fn release_lease(&self, id: &str) -> RpcResult<()> {
        self.leases()
            .release_lease(self.req(v1::ReleaseLeaseRequest {
                repo: self.repo(),
                id: id.into(),
            }))
            .await?;
        Ok(())
    }

    /// Lease ids of the repo.
    pub async fn list_leases(&self) -> RpcResult<Vec<String>> {
        let r = self
            .leases()
            .list_leases(self.req(v1::ListLeasesRequest {
                repo: self.repo(),
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(r.leases.into_iter().map(|l| l.id).collect())
    }

    /// Check runs of a changeset, as names.
    pub async fn list_checks(&self, changeset: &ObjectId) -> RpcResult<Vec<String>> {
        let r = v1::check_service_client::CheckServiceClient::new(self.ch.clone())
            .list_checks(self.req(v1::ListChecksRequest {
                repo: self.repo(),
                changeset: Some(pid(changeset)),
                ..Default::default()
            }))
            .await?
            .into_inner();
        Ok(r.checks.into_iter().map(|c| c.name).collect())
    }

    /// Subscribes to the repo's events of the given kinds from `from_seq`.
    pub async fn subscribe(
        &self,
        kinds: &[&str],
        from_seq: u64,
    ) -> RpcResult<tonic::Streaming<v1::Event>> {
        let mut req = self.req(v1::SubscribeRequest {
            repo: self.repo(),
            from_seq,
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            org: String::new(),
        });
        // A subscription outlives any unary deadline.
        req.set_timeout(Duration::from_secs(3600));
        Ok(
            v1::event_service_client::EventServiceClient::new(self.ch.clone())
                .subscribe(req)
                .await?
                .into_inner(),
        )
    }

    /// Events of the repo from `from_seq` (one page).
    pub async fn list_events(
        &self,
        kinds: &[&str],
        from_seq: u64,
    ) -> RpcResult<(Vec<v1::Event>, u64)> {
        let r = v1::event_service_client::EventServiceClient::new(self.ch.clone())
            .list_events(self.req(v1::ListEventsRequest {
                repo: self.repo(),
                org: String::new(),
                from_seq,
                kinds: kinds.iter().map(|k| k.to_string()).collect(),
                limit: 1000,
            }))
            .await?
            .into_inner();
        Ok((r.events, r.next_seq))
    }
}

/// The ref an event's `ref_updated` body names, with its target and version.
pub fn ref_updated(e: &v1::Event) -> Option<(String, Option<ObjectId>, u64)> {
    match &e.body {
        Some(v1::event::Body::RefUpdated(r)) => {
            Some((r.name.clone(), oid(r.target.as_ref()), r.version))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_decode_their_detail() {
        let mut d = bee_proto::errors::error_detail(
            v1::ErrorReason::VersionMismatch,
            "stale expected_version",
        );
        d.payload = Some(v1::error_detail::Payload::VersionMismatch(
            v1::VersionMismatch {
                r#ref: "ws/a".into(),
                expected_version: 1,
                current_version: 4,
                current_target: None,
            },
        ));
        let e = RpcError::from(bee_proto::errors::status_from_detail(&d));
        assert_eq!(e.reason(), "VERSION_MISMATCH");
        assert_eq!(e.current_version(), Some(4));
        assert!(!e.retryable());
        assert!(e.to_string().starts_with("VERSION_MISMATCH: "));
        let plain = RpcError::from(tonic::Status::unavailable("down"));
        assert!(plain.retryable());
        assert_eq!(plain.current_version(), None);
    }

    #[test]
    fn ids_round_trip() {
        let a = change_id(7);
        assert_ne!(a, change_id(8));
        let id = bee_object::blob_id(b"x");
        assert_eq!(oid(Some(&pid(&id))), Some(id));
        assert_eq!(oid(None), None);
    }
}
