//! The server, seen through the public `bee.v1` API only.
//!
//! [`Transport`] moves proto3-JSON messages (the Connect HTTP/JSON surface of SPEC §12.1); the
//! CLI implements it over its HTTP client. [`Remote`] adds the typed calls the working copy
//! needs: resolve a ref, list a sparse manifest, fetch and verify blobs, upload, create,
//! rebase and land changesets.

use std::collections::BTreeMap;

use bee_core::{ChangeId, Mode, ObjectId, ObjectKind};
use bee_proto::v1::{self, ErrorDetail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::{Result, WcError};
use crate::fsutil::path_text;
use crate::manifest::{Entry, Manifest};
use crate::sparse::Sparse;

/// Moves proto3-JSON messages to and from the server.
pub trait Transport {
    /// Calls a unary RPC (`/bee.v1.<Service>/<Method>`).
    fn unary(&self, path: &str, request: &Value) -> std::result::Result<Value, Box<ErrorDetail>>;

    /// Calls a server-streaming RPC and returns every message.
    fn server_stream(
        &self,
        path: &str,
        request: &Value,
    ) -> std::result::Result<Vec<Value>, Box<ErrorDetail>>;

    /// Calls a server-streaming RPC once per request and returns each call's messages, in
    /// request order. Transports that can run calls at once override this; by default they
    /// run one after another.
    fn server_streams(
        &self,
        path: &str,
        requests: &[Value],
    ) -> Vec<std::result::Result<Vec<Value>, Box<ErrorDetail>>> {
        requests
            .iter()
            .map(|r| self.server_stream(path, r))
            .collect()
    }

    /// `GetObjects` in binary protobuf, one call per request (several at once where the
    /// transport can), each call's chunks in request order. `None` when this transport only
    /// speaks proto3-JSON; callers then use [`Transport::server_streams`].
    #[allow(clippy::type_complexity)]
    fn get_objects(
        &self,
        requests: &[v1::GetObjectsRequest],
    ) -> Option<Vec<std::result::Result<Vec<v1::ObjectChunk>, Box<ErrorDetail>>>> {
        let _ = requests;
        None
    }

    /// `ListTree` in binary protobuf; `None` when this transport only speaks proto3-JSON
    /// (callers then use [`Transport::unary`]).
    fn list_tree(
        &self,
        request: &v1::ListTreeRequest,
    ) -> Option<std::result::Result<v1::ListTreeResponse, Box<ErrorDetail>>> {
        let _ = request;
        None
    }

    /// Calls a client-streaming RPC with these messages and returns the response.
    fn client_stream(
        &self,
        path: &str,
        requests: &[Value],
    ) -> std::result::Result<Value, Box<ErrorDetail>>;
}

/// Whether a listed object is redacted (quarantined or purged) or was unredacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedactionState {
    /// Quarantined or purged: unreadable.
    Redacted,
    /// Restored from quarantine.
    Unredacted,
}

/// Ids per `GetObjects` / `HasObjects` request.
const ID_BATCH: usize = 256;
/// Bytes per `UploadObjects` call, and per upload chunk.
const UPLOAD_BATCH_BYTES: usize = 8 << 20;
const UPLOAD_CHUNK_BYTES: usize = 1 << 20;
/// `ListTree` page size.
const PAGE: u32 = 1000;

/// A proto `ObjectId`.
pub fn pid(id: &ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// A domain id from a proto `ObjectId`.
pub fn from_pid(id: Option<&v1::ObjectId>, what: &str) -> Result<ObjectId> {
    let raw = id.map(|i| i.raw.as_slice()).unwrap_or_default();
    ObjectId::from_bytes(raw)
        .map_err(|e| WcError::internal("bad_response", format!("server sent a bad {what}: {e}")))
}

/// A proto `ChangeId`.
pub fn pchange(id: &ChangeId) -> v1::ChangeId {
    v1::ChangeId {
        raw: id.as_bytes().to_vec(),
    }
}

/// A domain ChangeId from a proto one.
pub fn from_pchange(id: Option<&v1::ChangeId>) -> Result<ChangeId> {
    let raw = id.map(|i| i.raw.as_slice()).unwrap_or_default();
    ChangeId::from_bytes(raw)
        .map_err(|e| WcError::internal("bad_response", format!("server sent a bad change id: {e}")))
}

/// The proto mode of a domain mode (`None` for gitlinks, which the proto enum lacks).
pub fn pmode(mode: Mode) -> Option<v1::Mode> {
    match mode {
        Mode::File => Some(v1::Mode::File),
        Mode::Executable => Some(v1::Mode::Executable),
        Mode::Symlink => Some(v1::Mode::Symlink),
        Mode::Tree => Some(v1::Mode::Tree),
        Mode::Gitlink => None,
    }
}

/// The domain mode of a proto mode value.
pub fn from_pmode(mode: i32) -> Option<Mode> {
    match v1::Mode::try_from(mode).ok()? {
        v1::Mode::File => Some(Mode::File),
        v1::Mode::Executable => Some(Mode::Executable),
        v1::Mode::Symlink => Some(Mode::Symlink),
        v1::Mode::Tree => Some(Mode::Tree),
        v1::Mode::Unspecified => None,
    }
}

/// A delta entry on the wire: a put of `entry`, or a delete when `None`.
pub fn delta_entry(path: &[u8], entry: Option<&Entry>) -> v1::DeltaEntry {
    v1::DeltaEntry {
        path: path.to_vec(),
        op: Some(match entry {
            Some(e) => v1::delta_entry::Op::Put(v1::Put {
                mode: pmode(e.mode).unwrap_or(v1::Mode::Unspecified) as i32,
                blob: Some(pid(&e.id)),
                size: e.size,
            }),
            None => v1::delta_entry::Op::Delete(true),
        }),
    }
}

/// A delta entry from the wire: `(path, Some(entry))` for a put, `(path, None)` for a delete.
pub fn from_delta_entry(d: &v1::DeltaEntry) -> Result<(Vec<u8>, Option<Entry>)> {
    match &d.op {
        Some(v1::delta_entry::Op::Put(p)) => {
            let mode = from_pmode(p.mode).ok_or_else(|| {
                WcError::internal("bad_response", "server sent a put without a mode")
            })?;
            Ok((
                d.path.clone(),
                Some(Entry {
                    mode,
                    id: from_pid(p.blob.as_ref(), "blob id")?,
                    size: p.size,
                }),
            ))
        }
        Some(v1::delta_entry::Op::Delete(_)) | None => Ok((d.path.clone(), None)),
    }
}

/// RFC 3339 (UTC) text of a Unix time, for proto3-JSON `Timestamp` fields.
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Author identity and time of a new changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Author name.
    pub name: String,
    /// Unix seconds.
    pub time: i64,
    /// Offset from UTC in minutes.
    pub tz_offset_minutes: i32,
}

impl Signature {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "time": rfc3339(self.time),
            "tzOffsetMinutes": self.tz_offset_minutes,
        })
    }
}

/// Fields of a new changeset.
#[derive(Clone, Debug)]
pub struct NewChangeset<'a> {
    /// Parent changeset.
    pub parent: ObjectId,
    /// Delta from the parent, sorted by path.
    pub delta: &'a [(Vec<u8>, Option<Entry>)],
    /// Stable ChangeId.
    pub change_id: ChangeId,
    /// Author and time.
    pub author: &'a Signature,
    /// Message.
    pub message: &'a str,
    /// Client provenance claims.
    pub provenance: Option<&'a v1::Provenance>,
    /// Allow an empty delta.
    pub allow_empty: bool,
}

/// Typed calls on one repo.
pub struct Remote<'t> {
    transport: &'t dyn Transport,
    repo: v1::RepoRef,
}

impl<'t> Remote<'t> {
    /// A remote for repo `org/name` (or a repo id).
    pub fn new(transport: &'t dyn Transport, repo: &str) -> Self {
        let repo = if repo.contains('/') {
            v1::repo_ref::Repo::FullName(repo.to_owned())
        } else {
            v1::repo_ref::Repo::Id(repo.to_owned())
        };
        Self {
            transport,
            repo: v1::RepoRef { repo: Some(repo) },
        }
    }

    fn repo_json(&self) -> Value {
        serde_json::to_value(&self.repo).unwrap_or(Value::Null)
    }

    fn call<Req: Serialize, Resp: DeserializeOwned>(&self, path: &str, req: &Req) -> Result<Resp> {
        let mut body = serde_json::to_value(req)
            .map_err(|e| WcError::internal("encode", format!("{path}: {e}")))?;
        if let Value::Object(map) = &mut body
            && !map.contains_key("repo")
        {
            map.insert("repo".into(), self.repo_json());
        }
        let resp = self
            .transport
            .unary(path, &body)
            .map_err(|d| WcError::remote(*d))?;
        decode(path, resp)
    }

    /// The ref's current target and version.
    pub fn resolve_ref(&self, name: &str, min_version: u64) -> Result<(ObjectId, u64)> {
        let resp: v1::ResolveRefResponse = self.call(
            "/bee.v1.ObjectService/ResolveRef",
            &v1::ResolveRefRequest {
                repo: Some(self.repo.clone()),
                name: name.to_owned(),
                min_version,
                at_version: 0,
            },
        )?;
        let r = resp
            .r#ref
            .ok_or_else(|| WcError::internal("bad_response", "ResolveRef returned no ref"))?;
        Ok((from_pid(r.target.as_ref(), "ref target")?, r.version))
    }

    /// Resolves a revision expression to a changeset id.
    pub fn resolve(&self, expr: &str) -> Result<ObjectId> {
        let resp: v1::ResolveResponse = self.call(
            "/bee.v1.QueryService/Resolve",
            &v1::ResolveRequest {
                repo: Some(self.repo.clone()),
                revisions: vec![v1::Revision {
                    expr: expr.to_owned(),
                }],
            },
        )?;
        let status = resp.statuses.first().copied().unwrap_or(0);
        if status != v1::ItemStatus::Ok as i32 || resp.ids.is_empty() {
            return Err(WcError::remote(ErrorDetail {
                reason: "NOT_FOUND".into(),
                message: format!("revision {expr:?} does not resolve"),
                ..Default::default()
            }));
        }
        from_pid(resp.ids.first(), "changeset id")
    }

    /// Changesets by id, with their meta when `include_meta`.
    pub fn changesets(&self, ids: &[ObjectId], include_meta: bool) -> Result<Vec<v1::Changeset>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let resp: v1::GetChangesetsResponse = self.call(
            "/bee.v1.QueryService/GetChangesets",
            &v1::GetChangesetsRequest {
                repo: Some(self.repo.clone()),
                ids: ids.iter().map(pid).collect(),
                include_meta,
            },
        )?;
        let mut out = Vec::with_capacity(ids.len());
        for (id, result) in ids.iter().zip(resp.results.iter()) {
            match &result.changeset {
                Some(cs) if result.status == v1::ItemStatus::Ok as i32 => out.push(cs.clone()),
                _ => {
                    return Err(WcError::remote(ErrorDetail {
                        reason: "NOT_FOUND".into(),
                        message: format!("changeset {id} is not readable"),
                        ..Default::default()
                    }));
                }
            }
        }
        if out.len() != ids.len() {
            return Err(WcError::internal(
                "bad_response",
                "GetChangesets returned fewer results than requested",
            ));
        }
        Ok(out)
    }

    fn list_tree_page(
        &self,
        at: &ObjectId,
        path: &[u8],
        recursive: bool,
        token: &str,
    ) -> Result<v1::ListTreeResponse> {
        let req = v1::ListTreeRequest {
            repo: Some(self.repo.clone()),
            at: Some(v1::Revision { expr: at.to_text() }),
            path: path.to_vec(),
            recursive,
            page: Some(v1::PageRequest {
                page_size: PAGE,
                page_token: token.to_owned(),
            }),
        };
        match self.transport.list_tree(&req) {
            Some(resp) => resp.map_err(|d| WcError::remote(*d)),
            None => self.call("/bee.v1.QueryService/ListTree", &req),
        }
    }

    fn list_all(&self, at: &ObjectId, path: &[u8], recursive: bool) -> Result<Vec<v1::TreeEntry>> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let page = self.list_tree_page(at, path, recursive, &token)?;
            out.extend(page.entries);
            token = page.page.map(|p| p.next_page_token).unwrap_or_default();
            if token.is_empty() {
                return Ok(out);
            }
        }
    }

    /// Every readable leaf of changeset `at` inside `sparse`, and how many entries were
    /// listed but not readable by this principal.
    pub fn manifest(&self, at: &ObjectId, sparse: &Sparse) -> Result<(Manifest, usize)> {
        let mut manifest = Manifest::new();
        let mut restricted = 0usize;
        let prefixes: Vec<Vec<u8>> = if sparse.is_full() {
            vec![Vec::new()]
        } else {
            sparse
                .prefixes()
                .iter()
                .map(|p| p.as_bytes().to_vec())
                .collect()
        };
        for prefix in prefixes {
            let entries = match self.list_all(at, &prefix, true) {
                Ok(e) => e,
                Err(e)
                    if !prefix.is_empty()
                        && (e.is_remote_reason("NOT_FOUND")
                            || e.is_remote_reason("INVALID_ARGUMENT")) =>
                {
                    // The prefix is a file or does not exist: look at its parent.
                    let parent = match prefix.iter().rposition(|&b| b == b'/') {
                        Some(i) => prefix[..i].to_vec(),
                        None => Vec::new(),
                    };
                    match self.list_all(at, &parent, false) {
                        Ok(list) => list.into_iter().filter(|t| t.path == prefix).collect(),
                        Err(e2)
                            if e2.is_remote_reason("NOT_FOUND")
                                || e2.is_remote_reason("INVALID_ARGUMENT") =>
                        {
                            Vec::new()
                        }
                        Err(e2) => return Err(e2),
                    }
                }
                Err(e) => return Err(e),
            };
            for t in entries {
                let Some(mode) = from_pmode(t.mode) else {
                    continue;
                };
                if mode == Mode::Tree {
                    continue;
                }
                if t.restricted {
                    restricted += 1;
                    continue;
                }
                manifest.insert(
                    t.path.clone(),
                    Entry {
                        mode,
                        id: from_pid(t.id.as_ref(), "tree entry id")?,
                        size: t.size,
                    },
                );
            }
        }
        Ok((manifest, restricted))
    }

    /// The leaf entry of each path at changeset `at` (`None` = absent or a directory), read
    /// one directory listing per distinct parent directory.
    pub fn entries_at(
        &self,
        at: &ObjectId,
        paths: &[Vec<u8>],
    ) -> Result<BTreeMap<Vec<u8>, Option<Entry>>> {
        let mut by_dir: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for p in paths {
            let dir = match p.iter().rposition(|&b| b == b'/') {
                Some(i) => p[..i].to_vec(),
                None => Vec::new(),
            };
            by_dir.entry(dir).or_default().push(p.clone());
        }
        let mut out = BTreeMap::new();
        for (dir, wanted) in by_dir {
            let listed = match self.list_all(at, &dir, false) {
                Ok(list) => list,
                Err(e)
                    if !dir.is_empty()
                        && (e.is_remote_reason("NOT_FOUND")
                            || e.is_remote_reason("INVALID_ARGUMENT")) =>
                {
                    Vec::new()
                }
                Err(e) => return Err(e),
            };
            let mut found: BTreeMap<Vec<u8>, Option<Entry>> = BTreeMap::new();
            for t in listed {
                let entry = match from_pmode(t.mode) {
                    Some(Mode::Tree) | None => None,
                    Some(mode) => Some(Entry {
                        mode,
                        id: from_pid(t.id.as_ref(), "tree entry id")?,
                        size: t.size,
                    }),
                };
                found.insert(t.path, entry);
            }
            for p in wanted {
                let e = found.get(&p).copied().flatten();
                out.insert(p, e);
            }
        }
        Ok(out)
    }

    /// Redaction records of the repo with an epoch at or above `since_epoch` (0 = all), and
    /// the repo's current redaction epoch (SPEC §13).
    pub fn redactions_since(
        &self,
        since_epoch: u64,
    ) -> Result<(Vec<(ObjectId, RedactionState)>, u64)> {
        let req = v1::ListRedactionsRequest {
            repo: Some(self.repo.clone()),
            since_epoch,
            ..Default::default()
        };
        let body =
            serde_json::to_value(&req).map_err(|e| WcError::internal("encode", e.to_string()))?;
        let resp = self
            .transport
            .unary("/bee.v1.AdminService/ListRedactions", &body)
            .map_err(|d| WcError::remote(*d))?;
        let resp: v1::ListRedactionsResponse = decode("ListRedactions", resp)?;
        let mut out = Vec::new();
        for r in &resp.redactions {
            let id = from_pid(r.id.as_ref(), "redaction id")?;
            let state = if r.state == v1::RedactionState::Unredacted as i32 {
                RedactionState::Unredacted
            } else {
                RedactionState::Redacted
            };
            out.push((id, state));
        }
        Ok((out, resp.current_epoch))
    }

    /// Fetches blobs and verifies each against its id. Ids the server will not serve
    /// (redacted, not readable) map to the reason it gave.
    pub fn blobs(
        &self,
        ids: &[ObjectId],
    ) -> Result<BTreeMap<ObjectId, std::result::Result<Vec<u8>, String>>> {
        let mut out = BTreeMap::new();
        let mut unique: Vec<ObjectId> = ids.to_vec();
        unique.sort();
        unique.dedup();
        let requests: Vec<v1::GetObjectsRequest> = unique
            .chunks(ID_BATCH)
            .map(|batch| v1::GetObjectsRequest {
                repo: Some(self.repo.clone()),
                ids: batch.iter().map(pid).collect(),
                ..Default::default()
            })
            .collect();
        // Binary where the transport speaks it; proto3-JSON otherwise.
        let replies = match self.transport.get_objects(&requests) {
            Some(replies) => replies,
            None => {
                let mut bodies = Vec::with_capacity(requests.len());
                for req in &requests {
                    bodies.push(
                        serde_json::to_value(req)
                            .map_err(|e| WcError::internal("encode", e.to_string()))?,
                    );
                }
                let mut out = Vec::with_capacity(bodies.len());
                for reply in self
                    .transport
                    .server_streams("/bee.v1.ObjectService/GetObjects", &bodies)
                {
                    out.push(match reply {
                        Ok(items) => Ok(items
                            .into_iter()
                            .map(|item| decode::<v1::ObjectChunk>("GetObjects", item))
                            .collect::<Result<Vec<_>>>()?),
                        Err(d) => Err(d),
                    });
                }
                out
            }
        };
        if replies.len() != requests.len() {
            return Err(WcError::internal(
                "bad_response",
                "the transport answered a different number of GetObjects calls than were made",
            ));
        }
        for (reply, batch) in replies.into_iter().zip(unique.chunks(ID_BATCH)) {
            let chunks = reply.map_err(|d| WcError::remote(*d))?;
            let mut partial: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
            for chunk in chunks {
                let id = from_pid(chunk.id.as_ref(), "object id")?;
                let ok = chunk.status == v1::ItemStatus::Ok as i32
                    || (chunk.status == 0 && chunk.error_reason.is_empty());
                if !ok {
                    let reason = if chunk.error_reason.is_empty() {
                        format!("status {}", chunk.status)
                    } else {
                        chunk.error_reason.clone()
                    };
                    out.insert(id, Err(reason));
                    continue;
                }
                if !chunk.signed_url.is_empty() {
                    out.insert(id, Err("SIGNED_URL_UNSUPPORTED".to_owned()));
                    continue;
                }
                if chunk.encoding == v1::ObjectEncoding::Zstd as i32 {
                    out.insert(id, Err("ZSTD_UNSUPPORTED".to_owned()));
                    continue;
                }
                let buf = partial.entry(id).or_default();
                if chunk.offset as usize != buf.len() {
                    return Err(WcError::integrity(
                        "bad_offset",
                        format!(
                            "object {id}: chunk at offset {} after {} bytes",
                            chunk.offset,
                            buf.len()
                        ),
                    ));
                }
                buf.extend_from_slice(&chunk.data);
                if chunk.last {
                    let bytes = partial.remove(&id).unwrap_or_default();
                    if id.kind() == ObjectKind::Blob && bee_object::blob_id(&bytes) != id {
                        return Err(WcError::integrity(
                            "id_mismatch",
                            format!("object {id}: bytes do not match the id"),
                        ));
                    }
                    out.insert(id, Ok(bytes));
                }
            }
            for id in batch {
                if !out.contains_key(id) {
                    out.insert(*id, Err("NOT_SERVED".to_owned()));
                }
            }
        }
        Ok(out)
    }

    /// Which of these objects the server already has (and the caller can read).
    pub fn has_objects(&self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(ID_BATCH) {
            let resp: v1::HasObjectsResponse = self.call(
                "/bee.v1.ObjectService/HasObjects",
                &v1::HasObjectsRequest {
                    repo: Some(self.repo.clone()),
                    ids: batch.iter().map(pid).collect(),
                },
            )?;
            if resp.present.len() != batch.len() {
                return Err(WcError::internal(
                    "bad_response",
                    "HasObjects answered a different number of ids",
                ));
            }
            out.extend(resp.present);
        }
        Ok(out)
    }

    /// Uploads blobs the server does not have yet; returns how many were sent.
    pub fn upload(&self, blobs: &[(ObjectId, Vec<u8>)]) -> Result<usize> {
        if blobs.is_empty() {
            return Ok(0);
        }
        let ids: Vec<ObjectId> = blobs.iter().map(|(id, _)| *id).collect();
        let present = self.has_objects(&ids)?;
        let missing: Vec<&(ObjectId, Vec<u8>)> = blobs
            .iter()
            .zip(present)
            .filter(|(_, p)| !p)
            .map(|(b, _)| b)
            .collect();
        let mut batch: Vec<Value> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut sent = 0usize;
        // A blob's chunks all go in one stream; streams are cut between blobs.
        for (id, bytes) in &missing {
            let mut offset = 0usize;
            loop {
                let end = (offset + UPLOAD_CHUNK_BYTES).min(bytes.len());
                let mut item = serde_json::to_value(v1::UploadItem {
                    repo: None,
                    id: Some(pid(id)),
                    offset: offset as u64,
                    data: bytes[offset..end].to_vec(),
                    last: end == bytes.len(),
                })
                .map_err(|e| WcError::internal("encode", e.to_string()))?;
                if batch.is_empty()
                    && let Value::Object(map) = &mut item
                {
                    map.insert("repo".into(), self.repo_json());
                }
                batch_bytes += end - offset;
                batch.push(item);
                offset = end;
                if offset >= bytes.len() {
                    break;
                }
            }
            sent += 1;
            if batch_bytes >= UPLOAD_BATCH_BYTES {
                self.flush_upload(&mut batch)?;
                batch_bytes = 0;
            }
        }
        self.flush_upload(&mut batch)?;
        Ok(sent)
    }

    fn flush_upload(&self, batch: &mut Vec<Value>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let resp = self
            .transport
            .client_stream("/bee.v1.WriteService/UploadObjects", batch)
            .map_err(|d| WcError::remote(*d))?;
        let _: v1::UploadObjectsResponse = decode("UploadObjects", resp)?;
        batch.clear();
        Ok(())
    }

    /// Creates a changeset (CreateChangeset) and returns it.
    pub fn create_changeset(&self, new: &NewChangeset<'_>) -> Result<v1::Changeset> {
        let req = v1::CreateChangesetRequest {
            repo: Some(self.repo.clone()),
            parents: vec![pid(&new.parent)],
            delta: new
                .delta
                .iter()
                .map(|(p, e)| delta_entry(p, e.as_ref()))
                .collect(),
            change_id: Some(pchange(&new.change_id)),
            author: new.author.name.clone(),
            message: new.message.to_owned(),
            provenance: new.provenance.cloned(),
            allow_empty: new.allow_empty,
            ..Default::default()
        };
        let mut body =
            serde_json::to_value(&req).map_err(|e| WcError::internal("encode", e.to_string()))?;
        if let Value::Object(map) = &mut body {
            map.insert("authorTime".into(), new.author.to_json());
        }
        let resp = self
            .transport
            .unary("/bee.v1.WriteService/CreateChangeset", &body)
            .map_err(|d| WcError::remote(*d))?;
        let resp: v1::CreateChangesetResponse = decode("CreateChangeset", resp)?;
        resp.changeset
            .ok_or_else(|| WcError::internal("bad_response", "CreateChangeset returned nothing"))
    }

    /// Rebases `changeset` onto `onto` server-side. Conflicts come back as a `MERGE_CONFLICT`
    /// error whose detail carries the conflict set.
    pub fn rebase(&self, changeset: &ObjectId, onto: &ObjectId) -> Result<v1::Changeset> {
        let resp: v1::RebaseResponse = self.call(
            "/bee.v1.WriteService/Rebase",
            &v1::RebaseRequest {
                repo: Some(self.repo.clone()),
                changeset: Some(pid(changeset)),
                onto: Some(v1::Revision {
                    expr: onto.to_text(),
                }),
                allow_empty: true,
            },
        )?;
        resp.changeset
            .ok_or_else(|| WcError::internal("bad_response", "Rebase returned nothing"))
    }

    /// Lands the stack ending at `head` on `ref_name`.
    pub fn land(&self, ref_name: &str, head: &ObjectId) -> Result<v1::LandResponse> {
        self.call(
            "/bee.v1.WriteService/Land",
            &v1::LandRequest {
                repo: Some(self.repo.clone()),
                r#ref: ref_name.to_owned(),
                head: Some(pid(head)),
                ..Default::default()
            },
        )
    }
}

fn decode<T: DeserializeOwned>(what: &str, value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| {
        WcError::internal(
            "bad_response",
            format!("server sent an invalid {what} response: {e}"),
        )
    })
}

/// Text of a path for messages.
pub fn show_path(path: &[u8]) -> String {
    path_text(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_281_600), "2026-10-18T00:00:00Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn delta_entries_roundtrip() {
        let e = Entry::for_bytes(Mode::Executable, b"x");
        let put = delta_entry(b"bin/x", Some(&e));
        assert_eq!(
            from_delta_entry(&put).expect("put"),
            (b"bin/x".to_vec(), Some(e))
        );
        let del = delta_entry(b"gone", None);
        assert_eq!(
            from_delta_entry(&del).expect("del"),
            (b"gone".to_vec(), None)
        );
        let json = serde_json::to_value(&del).expect("json");
        assert_eq!(json["delete"], true);
    }
}
