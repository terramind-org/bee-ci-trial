//! An in-memory stand-in for the server's VCS API (flat trees, the SPEC §7.3 per-path rebase
//! rules), used by this crate's unit tests. The real server is exercised by the CLI's
//! integration tests.

use std::cell::RefCell;
use std::collections::BTreeMap;

use bee_core::{ChangeId, Mode, ObjectId, ObjectKind};
use bee_proto::v1::{self, ErrorDetail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::manifest::{Entry, Manifest};
use crate::remote::{Transport, delta_entry, from_delta_entry, from_pid, pchange, pid, pmode};

/// One stored changeset.
#[derive(Clone, Debug)]
pub struct Stored {
    pub parent: Option<ObjectId>,
    pub tree: Manifest,
    pub delta: Vec<(Vec<u8>, Option<Entry>)>,
    pub change_id: ChangeId,
    pub message: String,
    pub author: String,
}

/// The fake server's data.
#[derive(Default)]
pub struct Data {
    pub changesets: BTreeMap<ObjectId, Stored>,
    pub blobs: BTreeMap<ObjectId, Vec<u8>>,
    pub refs: BTreeMap<String, (ObjectId, u64)>,
    pub landed: BTreeMap<ChangeId, ObjectId>,
    pub calls: Vec<String>,
    /// Redacted (true) or unredacted (false) objects with the epoch of their last change.
    pub redactions: BTreeMap<ObjectId, (bool, u64)>,
    pub redaction_epoch: u64,
}

/// A fake server holding one repo.
pub struct Fake {
    pub data: RefCell<Data>,
}

fn err(reason: &str, message: &str) -> Box<ErrorDetail> {
    Box::new(ErrorDetail {
        reason: reason.into(),
        message: message.into(),
        ..Default::default()
    })
}

fn parse<T: DeserializeOwned>(v: &Value) -> Result<T, Box<ErrorDetail>> {
    serde_json::from_value(v.clone()).map_err(|e| err("INVALID_ARGUMENT", &e.to_string()))
}

fn out<T: Serialize>(v: &T) -> Result<Value, Box<ErrorDetail>> {
    serde_json::to_value(v).map_err(|e| err("INTERNAL", &e.to_string()))
}

fn id_of(v: Option<&v1::ObjectId>) -> Result<ObjectId, Box<ErrorDetail>> {
    from_pid(v, "id").map_err(|e| err("INVALID_ARGUMENT", &e.message))
}

impl Fake {
    /// A repo whose `main` points at an empty root changeset.
    pub fn new() -> Self {
        let root = Stored {
            parent: None,
            tree: Manifest::new(),
            delta: Vec::new(),
            change_id: ChangeId::new([0; 16]),
            message: String::new(),
            author: String::new(),
        };
        let id = Self::hash(&root);
        let mut data = Data::default();
        data.changesets.insert(id, root);
        data.refs.insert("main".into(), (id, 1));
        Fake {
            data: RefCell::new(data),
        }
    }

    fn hash(s: &Stored) -> ObjectId {
        let mut input = Vec::new();
        if let Some(p) = &s.parent {
            input.extend_from_slice(p.as_bytes());
        }
        for (p, e) in &s.tree {
            input.extend_from_slice(p);
            input.push(0);
            input.extend_from_slice(e.id.as_bytes());
            input.push(e.mode.as_u64() as u8);
        }
        input.extend_from_slice(s.change_id.as_bytes());
        input.extend_from_slice(s.message.as_bytes());
        input.extend_from_slice(s.author.as_bytes());
        bee_object::hash_id(ObjectKind::Changeset, &input)
    }

    /// The tip of `main`.
    pub fn tip(&self) -> (ObjectId, u64) {
        self.data.borrow().refs["main"]
    }

    /// The tree of a changeset.
    pub fn tree(&self, id: &ObjectId) -> Manifest {
        self.data.borrow().changesets[id].tree.clone()
    }

    /// Writes files straight onto `main` (as another agent would).
    pub fn push(&self, files: &[(&str, Option<&str>)], message: &str) -> ObjectId {
        let (tip, version) = self.tip();
        let mut data = self.data.borrow_mut();
        let mut tree = data.changesets[&tip].tree.clone();
        let mut delta = Vec::new();
        for (p, content) in files {
            match content {
                Some(c) => {
                    let e = Entry::for_bytes(Mode::File, c.as_bytes());
                    data.blobs.insert(e.id, c.as_bytes().to_vec());
                    tree.insert(p.as_bytes().to_vec(), e);
                    delta.push((p.as_bytes().to_vec(), Some(e)));
                }
                None => {
                    tree.remove(p.as_bytes());
                    delta.push((p.as_bytes().to_vec(), None));
                }
            }
        }
        let s = Stored {
            parent: Some(tip),
            tree,
            delta,
            change_id: ChangeId::new(
                *bee_object::digest(message.as_bytes())
                    .first_chunk()
                    .unwrap_or(&[1; 16]),
            ),
            message: message.into(),
            author: "other".into(),
        };
        let id = Self::hash(&s);
        data.changesets.insert(id, s);
        data.refs.insert("main".into(), (id, version + 1));
        id
    }

    /// Redacts the blob holding `content` (true) or unredacts it (false), bumping the epoch.
    pub fn set_redacted(&self, content: &str, redacted: bool) {
        let id = bee_object::blob_id(content.as_bytes());
        let mut data = self.data.borrow_mut();
        data.redaction_epoch += 1;
        let epoch = data.redaction_epoch;
        data.redactions.insert(id, (redacted, epoch));
    }

    fn is_redacted(&self, id: &ObjectId) -> bool {
        self.data
            .borrow()
            .redactions
            .get(id)
            .is_some_and(|(r, _)| *r)
    }

    fn view(&self, id: &ObjectId) -> v1::Changeset {
        let data = self.data.borrow();
        let s = &data.changesets[id];
        v1::Changeset {
            id: Some(pid(id)),
            parents: s.parent.iter().map(pid).collect(),
            change_id: Some(pchange(&s.change_id)),
            delta: s
                .delta
                .iter()
                .map(|(p, e)| delta_entry(p, e.as_ref()))
                .collect(),
            meta: Some(v1::Meta {
                author: s.author.clone(),
                message: s.message.clone(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn store(&self, s: Stored) -> ObjectId {
        let id = Self::hash(&s);
        self.data.borrow_mut().changesets.insert(id, s);
        id
    }

    /// SPEC §7.3 per-path rules over flat trees.
    fn rebase_one(&self, c: &ObjectId, onto: &ObjectId) -> Result<ObjectId, Box<ErrorDetail>> {
        let (cs, base_tree, onto_tree) = {
            let data = self.data.borrow();
            let cs = data
                .changesets
                .get(c)
                .cloned()
                .ok_or_else(|| err("NOT_FOUND", "no changeset"))?;
            if cs.parent == Some(*onto) {
                return Ok(*c);
            }
            let base = cs
                .parent
                .map(|p| data.changesets[&p].tree.clone())
                .unwrap_or_default();
            let onto_tree = data
                .changesets
                .get(onto)
                .ok_or_else(|| err("NOT_FOUND", "no onto"))?
                .tree
                .clone();
            (cs, base, onto_tree)
        };
        let mut tree = onto_tree.clone();
        let mut conflicts = Vec::new();
        let mut clean = Vec::new();
        for (p, _) in &cs.delta {
            let b = base_tree.get(p).copied();
            let o = onto_tree.get(p).copied();
            let t = cs.tree.get(p).copied();
            let res = if o == b {
                Some(t)
            } else if t == b || o == t {
                Some(o)
            } else {
                None
            };
            match res {
                Some(v) => {
                    match v {
                        Some(e) => tree.insert(p.clone(), e),
                        None => tree.remove(p),
                    };
                    clean.push(delta_entry(p, v.as_ref()));
                }
                None => {
                    let side = |e: Option<Entry>| {
                        e.map(|e| v1::ConflictSide {
                            mode: pmode(e.mode).unwrap_or(v1::Mode::File) as i32,
                            blob: Some(pid(&e.id)),
                        })
                    };
                    let kind = match (b, o, t) {
                        (None, _, _) => v1::ConflictKind::AddAdd,
                        (_, None, _) | (_, _, None) => v1::ConflictKind::ModifyDelete,
                        _ => v1::ConflictKind::Content,
                    };
                    conflicts.push(v1::Conflict {
                        path: p.clone(),
                        kind: kind as i32,
                        base: side(b),
                        ours: side(o),
                        theirs: side(t),
                        hunks: Vec::new(),
                    });
                }
            }
        }
        if !conflicts.is_empty() {
            let mut d = err("MERGE_CONFLICT", "conflicts");
            d.payload = Some(v1::error_detail::Payload::Conflicts(v1::ConflictSet {
                conflicts,
                clean_entries: clean,
                base: cs.parent.as_ref().map(pid),
                ours: Some(pid(onto)),
                theirs: Some(pid(c)),
            }));
            return Err(d);
        }
        let delta = crate::manifest::diff(&onto_tree, &tree)
            .into_iter()
            .map(|ch| (ch.path, ch.new))
            .collect();
        Ok(self.store(Stored {
            parent: Some(*onto),
            tree,
            delta,
            change_id: cs.change_id,
            message: cs.message,
            author: cs.author,
        }))
    }

    fn handle(&self, path: &str, req: &Value) -> Result<Value, Box<ErrorDetail>> {
        self.data.borrow_mut().calls.push(path.to_owned());
        let method = path.rsplit('/').next().unwrap_or_default();
        match method {
            "ResolveRef" => {
                let r: v1::ResolveRefRequest = parse(req)?;
                let (id, version) = *self
                    .data
                    .borrow()
                    .refs
                    .get(&r.name)
                    .ok_or_else(|| err("NOT_FOUND", "no ref"))?;
                out(&v1::ResolveRefResponse {
                    r#ref: Some(v1::Ref {
                        name: r.name,
                        target: Some(pid(&id)),
                        version,
                    }),
                    ..Default::default()
                })
            }
            "Resolve" => {
                let r: v1::ResolveRequest = parse(req)?;
                let expr = r
                    .revisions
                    .first()
                    .map(|x| x.expr.clone())
                    .unwrap_or_default();
                let data = self.data.borrow();
                let found = data.refs.get(&expr).map(|(id, _)| *id).or_else(|| {
                    data.changesets
                        .keys()
                        .find(|id| id.to_text().starts_with(&expr))
                        .copied()
                });
                out(&v1::ResolveResponse {
                    ids: found.iter().map(pid).collect(),
                    statuses: vec![if found.is_some() { 1 } else { 2 }],
                })
            }
            "ListTree" => {
                let r: v1::ListTreeRequest = parse(req)?;
                let at = r.at.map(|a| a.expr).unwrap_or_default();
                let id = ObjectId::parse(&at).map_err(|e| err("INVALID_ARGUMENT", &e.message))?;
                let tree = self.tree(&id);
                let mut entries = Vec::new();
                let prefix = r.path.clone();
                let mut dirs = std::collections::BTreeSet::new();
                let mut any = prefix.is_empty();
                for (p, e) in &tree {
                    let rel = if prefix.is_empty() {
                        p.as_slice()
                    } else if p.len() > prefix.len()
                        && p.starts_with(&prefix)
                        && p[prefix.len()] == b'/'
                    {
                        &p[prefix.len() + 1..]
                    } else {
                        continue;
                    };
                    any = true;
                    if !r.recursive
                        && let Some(i) = rel.iter().position(|&b| b == b'/')
                    {
                        let mut d = prefix.clone();
                        if !d.is_empty() {
                            d.push(b'/');
                        }
                        d.extend_from_slice(&rel[..i]);
                        dirs.insert(d);
                        continue;
                    }
                    entries.push(v1::TreeEntry {
                        path: p.clone(),
                        mode: pmode(e.mode).unwrap_or(v1::Mode::File) as i32,
                        id: Some(pid(&e.id)),
                        size: e.size,
                        restricted: false,
                    });
                }
                if !any {
                    return Err(err("NOT_FOUND", "no such directory"));
                }
                for d in dirs {
                    entries.push(v1::TreeEntry {
                        path: d,
                        mode: v1::Mode::Tree as i32,
                        id: Some(pid(&ObjectId::new(ObjectKind::Tree, [0; 32]))),
                        size: 0,
                        restricted: false,
                    });
                }
                out(&v1::ListTreeResponse {
                    entries,
                    page: None,
                })
            }
            "GetChangesets" => {
                let r: v1::GetChangesetsRequest = parse(req)?;
                let mut resp = v1::GetChangesetsResponse::default();
                for raw in &r.ids {
                    let id = id_of(Some(raw))?;
                    let cs = self.view(&id);
                    resp.changesets.push(cs.clone());
                    resp.results.push(v1::ChangesetResult {
                        id: Some(raw.clone()),
                        status: 1,
                        changeset: Some(cs),
                    });
                }
                out(&resp)
            }
            "ListRedactions" => {
                let r: v1::ListRedactionsRequest = parse(req)?;
                let data = self.data.borrow();
                let redactions = data
                    .redactions
                    .iter()
                    .filter(|(_, (_, epoch))| *epoch >= r.since_epoch)
                    .map(|(id, (redacted, epoch))| v1::Redaction {
                        id: Some(pid(id)),
                        epoch: *epoch,
                        state: if *redacted {
                            v1::RedactionState::Quarantined as i32
                        } else {
                            v1::RedactionState::Unredacted as i32
                        },
                        ..Default::default()
                    })
                    .collect();
                out(&v1::ListRedactionsResponse {
                    redactions,
                    current_epoch: data.redaction_epoch,
                    page: None,
                })
            }
            "HasObjects" => {
                let r: v1::HasObjectsRequest = parse(req)?;
                let data = self.data.borrow();
                let present = r
                    .ids
                    .iter()
                    .map(|i| id_of(Some(i)).map(|id| data.blobs.contains_key(&id)))
                    .collect::<Result<_, _>>()?;
                out(&v1::HasObjectsResponse { present })
            }
            "CreateChangeset" => {
                let r: v1::CreateChangesetRequest = parse(req)?;
                let parent = id_of(r.parents.first())?;
                let mut tree = self.tree(&parent);
                let mut delta = Vec::new();
                for d in &r.delta {
                    let (p, e) =
                        from_delta_entry(d).map_err(|e| err("INVALID_ARGUMENT", &e.message))?;
                    if let Some(e) = &e
                        && !self.data.borrow().blobs.contains_key(&e.id)
                    {
                        return Err(err("MISSING_OBJECT", "blob not uploaded"));
                    }
                    if tree.get(&p).copied() == e {
                        let mut d = err("INVALID_ARGUMENT", "no-op delta entry");
                        d.subcode = "noop_put".into();
                        return Err(d);
                    }
                    match e {
                        Some(e) => tree.insert(p.clone(), e),
                        None => tree.remove(&p),
                    };
                    delta.push((p, e));
                }
                if delta.is_empty() && !r.allow_empty {
                    return Err(err("EMPTY_CHANGE", "empty"));
                }
                let change_id = crate::remote::from_pchange(r.change_id.as_ref())
                    .map_err(|e| err("INVALID_ARGUMENT", &e.message))?;
                let id = self.store(Stored {
                    parent: Some(parent),
                    tree,
                    delta,
                    change_id,
                    message: r.message,
                    author: r.author,
                });
                out(&v1::CreateChangesetResponse {
                    changeset: Some(self.view(&id)),
                })
            }
            "Rebase" => {
                let r: v1::RebaseRequest = parse(req)?;
                let c = id_of(r.changeset.as_ref())?;
                let onto = ObjectId::parse(&r.onto.map(|o| o.expr).unwrap_or_default())
                    .map_err(|e| err("INVALID_ARGUMENT", &e.message))?;
                let id = self.rebase_one(&c, &onto)?;
                out(&v1::RebaseResponse {
                    changeset: Some(self.view(&id)),
                })
            }
            "Land" => {
                let r: v1::LandRequest = parse(req)?;
                let head = id_of(r.head.as_ref())?;
                let (tip, version) = self.tip();
                // The stack: first-parent chain from head back to an ancestor of the tip.
                let mut chain = Vec::new();
                let mut cur = Some(head);
                let ancestors: std::collections::BTreeSet<ObjectId> = {
                    let data = self.data.borrow();
                    let mut set = std::collections::BTreeSet::new();
                    let mut a = Some(tip);
                    while let Some(x) = a {
                        set.insert(x);
                        a = data.changesets[&x].parent;
                    }
                    set
                };
                while let Some(c) = cur {
                    if ancestors.contains(&c) {
                        break;
                    }
                    chain.push(c);
                    cur = self.data.borrow().changesets[&c].parent;
                }
                chain.reverse();
                let mut base = tip;
                let mut landed = Vec::new();
                for c in chain {
                    let cid = self.data.borrow().changesets[&c].change_id;
                    let new = self.rebase_one(&c, &base)?;
                    self.data.borrow_mut().landed.insert(cid, new);
                    landed.push(v1::LandedChange {
                        change_id: Some(pchange(&cid)),
                        source: Some(pid(&c)),
                        result: Some(pid(&new)),
                        skipped: false,
                    });
                    base = new;
                }
                self.data
                    .borrow_mut()
                    .refs
                    .insert(r.r#ref.clone(), (base, version + 1));
                out(&v1::LandResponse {
                    r#ref: Some(v1::Ref {
                        name: r.r#ref,
                        target: Some(pid(&base)),
                        version: version + 1,
                    }),
                    landed,
                    attempts: 1,
                    event_seq: version + 1,
                })
            }
            _ => Err(err("UNIMPLEMENTED", path)),
        }
    }
}

impl Transport for Fake {
    fn unary(&self, path: &str, request: &Value) -> Result<Value, Box<ErrorDetail>> {
        self.handle(path, request)
    }

    fn server_stream(&self, path: &str, request: &Value) -> Result<Vec<Value>, Box<ErrorDetail>> {
        self.data.borrow_mut().calls.push(path.to_owned());
        let r: v1::GetObjectsRequest = parse(request)?;
        let mut items = Vec::new();
        for raw in &r.ids {
            let id = id_of(Some(raw))?;
            if self.is_redacted(&id) {
                items.push(out(&v1::ObjectChunk {
                    id: Some(raw.clone()),
                    last: true,
                    status: v1::ItemStatus::Redacted as i32,
                    error_reason: "REDACTED".into(),
                    ..Default::default()
                })?);
                continue;
            }
            let chunk = match self.data.borrow().blobs.get(&id) {
                Some(b) => v1::ObjectChunk {
                    id: Some(raw.clone()),
                    data: b.clone(),
                    last: true,
                    status: 1,
                    ..Default::default()
                },
                None => v1::ObjectChunk {
                    id: Some(raw.clone()),
                    last: true,
                    status: 2,
                    error_reason: "NOT_FOUND".into(),
                    ..Default::default()
                },
            };
            items.push(out(&chunk)?);
        }
        Ok(items)
    }

    fn client_stream(&self, path: &str, requests: &[Value]) -> Result<Value, Box<ErrorDetail>> {
        self.data.borrow_mut().calls.push(path.to_owned());
        let mut partial: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
        let mut stored = Vec::new();
        for r in requests {
            let item: v1::UploadItem = parse(r)?;
            let id = id_of(item.id.as_ref())?;
            let buf = partial.entry(id).or_default();
            buf.extend_from_slice(&item.data);
            if item.last {
                let bytes = partial.remove(&id).unwrap_or_default();
                if bee_object::blob_id(&bytes) != id {
                    return Err(err("INTEGRITY", "bad blob"));
                }
                self.data.borrow_mut().blobs.insert(id, bytes);
                stored.push(pid(&id));
            }
        }
        out(&v1::UploadObjectsResponse {
            stored,
            already_present: Vec::new(),
        })
    }
}
