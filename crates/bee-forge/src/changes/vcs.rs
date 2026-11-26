//! What change requests need from the VCS, and the implementation over `bee-history`.
//!
//! The changes module never touches the VCS stores directly: it goes through [`ChangeVcs`]
//! (reads, classification, the changesets it creates for suggestions, rebases and reverts)
//! and [`Lander`] (landing). The dependency points forge → VCS only (SPEC §15.1).
//!
//! [`HistoryVcs`] implements both over a [`bee_store::Writer`]. Its [`Lander`] is the
//! reference land of SPEC §7.4 steps 1–5 (stack walk, `landed` lookups, rebase, ref CAS with
//! retries) without the coalescer; the server routes protected refs through the land
//! coalescer instead and installs its own [`Lander`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bee_classify::ClassRules;
use bee_core::{ChangeId as CoreChangeId, Mode, ObjectId as CoreId, RefName, RepoPath, Ulid};
use bee_derived::{Derived, DiffClassType, Registry as DerivedRegistry};
use bee_history::{
    DiffOptions, FileChange, History, MergeOptions, MergeOutcome, RebaseOptions, RebaseOutcome,
    StackOutcome,
};
use bee_object::{DeltaEntry, Meta, Provenance, Timestamp, blob_id};
use bee_proto::v1::{
    ChangeId, Conflict, ConflictHunk, ConflictKind, ConflictSet, ConflictSide, DiffHunk, DiffItem,
    DiffStatus, LandMethod, LandResponse, LandedChange, ObjectId, Ref, Repo,
};
use bee_store::{Expected, LandedEntry, OrgId, RefStep, RefUpdate, RepoId, UploadRecord, Writer};

use crate::error::{ForgeError, Result};
use crate::ids::Clock;

/// Most changesets in one CR stack.
pub const MAX_STACK: usize = 1000;
/// Land attempts before `LAND_CONTENTION`.
pub const LAND_ATTEMPTS: u32 = 8;

/// Merge-queue limits supplied by matching active rulesets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueParams {
    /// Maximum queued entries on one target ref; zero means no ruleset limit.
    pub max_entries: u32,
}

/// A stack: the first-parent chain from a head back to the first ancestor of a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StackInfo {
    /// Members, oldest first.
    pub members: Vec<ObjectId>,
    /// The parent of the oldest member (an ancestor of the target).
    pub base: ObjectId,
    /// ChangeIds of the members, oldest first.
    pub change_ids: Vec<ChangeId>,
    /// Some member is a merge changeset.
    pub has_merges: bool,
}

/// One changeset's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitInfo {
    /// Its id.
    pub id: ObjectId,
    /// Parents.
    pub parents: Vec<ObjectId>,
    /// ChangeId.
    pub change_id: ChangeId,
    /// Message (empty when the meta is redacted).
    pub message: String,
    /// Author.
    pub author: String,
}

/// One classified path of a stack.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassifiedPath {
    /// Path.
    pub path: String,
    /// Diff class wire name.
    pub class: String,
    /// New size at the head; `None` when the path is absent there.
    pub size: Option<u64>,
}

/// The classification of a stack: every path its members change, with the highest class
/// over the members, and the highest class overall (empty for an empty change).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Classified {
    /// Highest class.
    pub highest: String,
    /// Per path, sorted.
    pub paths: Vec<ClassifiedPath>,
}

/// A changeset to create on top of `parent`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewCommit {
    /// Parent (`None` = a root changeset).
    pub parent: Option<ObjectId>,
    /// Files to write (`Some`) or delete (`None`).
    pub edits: Vec<(String, Option<Vec<u8>>)>,
    /// Message.
    pub message: String,
    /// Author line.
    pub author: String,
    /// Creating principal id.
    pub creator: String,
    /// Provenance chain (actor first).
    pub chain: Vec<String>,
    /// ChangeId (raw 16 bytes); `None` derives one from the parent, edits and message.
    pub change_id: Option<Vec<u8>>,
}

/// The result of a rebase or revert.
#[derive(Clone, Debug, PartialEq)]
pub enum Rebased {
    /// The new head.
    Head(ObjectId),
    /// Conflicts as data.
    Conflict(ConflictSet),
}

/// What change requests need from the VCS.
#[async_trait]
pub trait ChangeVcs: Send + Sync {
    /// The ref's current target and version; `None` when the ref is absent or deleted.
    async fn ref_tip(&self, repo: &Repo, name: &str) -> Result<Option<Ref>>;
    /// The stack from `head` back to the first ancestor of `onto`. `EMPTY_CHANGE` when
    /// `head` is already an ancestor of `onto`.
    async fn stack(&self, repo: &Repo, head: &ObjectId, onto: &ObjectId) -> Result<StackInfo>;
    /// A changeset's metadata.
    async fn commit(&self, repo: &Repo, id: &ObjectId) -> Result<CommitInfo>;
    /// Classifies the paths of a stack (SPEC §11.3) under `rules`; `fingerprint` changes
    /// whenever the rules do.
    async fn classify(
        &self,
        repo: &Repo,
        members: &[ObjectId],
        rules: &ClassRules,
        fingerprint: &str,
    ) -> Result<Classified>;
    /// The diff from `from` to `to`, limited to `paths` when not empty, with renames.
    async fn diff(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
        paths: &[String],
        hunks: bool,
        context: u32,
    ) -> Result<Vec<DiffItem>>;
    /// [`ChangeVcs::diff`] that, with `ignore_whitespace`, reports no whitespace-only line
    /// changes (and omits files that have nothing else). The default ignores the flag.
    #[allow(clippy::too_many_arguments)]
    async fn diff_with(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
        paths: &[String],
        hunks: bool,
        context: u32,
        ignore_whitespace: bool,
    ) -> Result<Vec<DiffItem>> {
        let _ = ignore_whitespace;
        self.diff(repo, from, to, paths, hunks, context).await
    }
    /// A file's bytes at a changeset; `None` when absent or not a file.
    async fn read_file(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<Vec<u8>>>;
    /// A file's blob id at a changeset; `None` when absent.
    async fn file_id(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<ObjectId>>;
    /// Names of the files (not directories) directly in `dir`; empty when absent.
    async fn list_dir(&self, repo: &Repo, at: &ObjectId, dir: &str) -> Result<Vec<String>>;
    /// Creates a changeset.
    async fn create_commit(&self, repo: &Repo, c: NewCommit) -> Result<ObjectId>;
    /// Rebases a stack (oldest first) onto `onto`.
    async fn rebase_stack(
        &self,
        repo: &Repo,
        members: &[ObjectId],
        onto: &ObjectId,
    ) -> Result<Rebased>;
    /// A changeset on `onto` that restores the content `restore` had at the paths that
    /// differ between `restore` and `landed` (a revert of `restore..landed`).
    async fn revert(
        &self,
        repo: &Repo,
        restore: &ObjectId,
        landed: &ObjectId,
        onto: &ObjectId,
        commit: NewCommit,
    ) -> Result<Rebased>;
    /// Whether `a` is an ancestor of (or equal to) `b`.
    async fn is_ancestor(&self, repo: &Repo, a: &ObjectId, b: &ObjectId) -> Result<bool>;
    /// A merge changeset with parents `head` and `onto` ("update base" by merge commit).
    async fn merge(
        &self,
        repo: &Repo,
        head: &ObjectId,
        onto: &ObjectId,
        commit: NewCommit,
    ) -> Result<Rebased>;
}

/// Land facts the changes module cannot compute itself (SPEC §9.4): signature states of the
/// stack (verification needs the git gateway and the key registry), enforced org-required
/// workflows (CI) and open code-scanning alerts (security). The server installs one with
/// [`crate::changes::Changes::set_land_facts`]; without one, changesets count as unsigned and
/// there are no workflow gates or alerts.
#[async_trait]
pub trait LandFacts: Send + Sync {
    /// The signature state of each changeset of `stack`, in order.
    async fn signatures(
        &self,
        repo: &Repo,
        stack: &[ObjectId],
    ) -> Result<Vec<bee_policy::SignatureState>>;
    /// Enforced required workflows for landing `heads` (the CR head first).
    async fn required_workflows(
        &self,
        repo: &Repo,
        heads: &[ObjectId],
    ) -> Result<Vec<bee_policy::WorkflowGate>>;
    /// Open code-scanning alerts on `heads` or on `target_ref`.
    async fn code_alerts(
        &self,
        repo: &Repo,
        target_ref: &str,
        heads: &[ObjectId],
    ) -> Result<Vec<bee_policy::CodeAlert>>;
    /// Whether an active ruleset with a merge-queue rule matches `target_ref` (rulesets live
    /// in the server store), so the ref lands only through the merge queue, exactly as with
    /// the repo setting `merge_queue_required`. Without rulesets: `false`.
    async fn merge_queue_required(&self, _repo: &Repo, _target_ref: &str) -> Result<bool> {
        Ok(false)
    }
    /// Queue limits from matching active rulesets. Without rulesets: no limits.
    async fn merge_queue_params(
        &self,
        _repo: &Repo,
        _target_ref: &str,
    ) -> Result<Option<QueueParams>> {
        Ok(None)
    }
}

/// A land request for one CR (SPEC §7.4; `LandRequest` with the CR's stack head).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandIntent {
    /// Target ref.
    pub ref_name: String,
    /// Stack head.
    pub head: ObjectId,
    /// Expected ref version (0 = any).
    pub expected_version: u64,
    /// Land method.
    pub method: LandMethod,
    /// Squash or merge-commit message.
    pub message: String,
    /// The CR landed.
    pub change_request_id: String,
    /// The landing principal (actor first).
    pub chain: Vec<String>,
    /// Kind of the acting principal from the authenticated landing context.
    pub actor_kind: bee_proto::v1::PrincipalKind,
    /// ChangeId of the squash or merge changeset (raw), when the caller picks one.
    pub change_id: Option<Vec<u8>>,
}

/// Lands stacks onto refs. The server's implementation routes through the land coalescer.
#[async_trait]
pub trait Lander: Send + Sync {
    /// Lands `intent` and reports the result.
    async fn land(&self, repo: &Repo, intent: &LandIntent) -> Result<LandResponse>;
}

/// A lander for deployments where landing is served elsewhere: always `UNIMPLEMENTED` /
/// `served_elsewhere`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLander;

#[async_trait]
impl Lander for NoLander {
    async fn land(&self, _repo: &Repo, _intent: &LandIntent) -> Result<LandResponse> {
        Err(crate::service::unserved(
            "Land",
            "the land coalescer (bee-server)",
        ))
    }
}

/// Converts a VCS error into a forge error with the same reason.
pub fn vcs_err(e: bee_core::BeeError) -> ForgeError {
    match bee_proto::errors::parse_reason(e.reason.as_str()) {
        Some(r) => ForgeError::new(r, e.message),
        None => ForgeError::unavailable(format!("vcs: {}", e.message)),
    }
}

fn core_id(id: &ObjectId) -> Result<CoreId> {
    CoreId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

fn proto_id(id: &CoreId) -> ObjectId {
    ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

fn proto_change(c: &CoreChangeId) -> ChangeId {
    ChangeId {
        raw: c.as_bytes().to_vec(),
    }
}

fn repo_path(p: &str) -> Result<RepoPath> {
    RepoPath::new(p.as_bytes().to_vec())
        .map_err(|e| ForgeError::invalid("invalid_path", format!("{p:?}: {}", e.message)))
}

fn ulid(s: &str) -> Result<Ulid> {
    Ulid::parse(s).map_err(|e| ForgeError::invalid("invalid_id", e.message))
}

fn proto_mode(m: Mode) -> i32 {
    use bee_proto::v1::Mode as P;
    (match m {
        Mode::File => P::File,
        Mode::Executable => P::Executable,
        Mode::Symlink => P::Symlink,
        Mode::Tree => P::Tree,
        Mode::Gitlink => P::Unspecified,
    }) as i32
}

/// The `bee.v1.ConflictSet` of a history conflict.
pub fn conflict_proto(
    c: &bee_history::ConflictSet,
    ours: Option<&ObjectId>,
    theirs: Option<&ObjectId>,
) -> ConflictSet {
    use bee_merge::ConflictKind as K;
    let side = |s: &Option<bee_history::ConflictSide>| {
        s.as_ref().map(|s| ConflictSide {
            mode: proto_mode(s.mode),
            blob: s.id.as_ref().map(proto_id),
        })
    };
    ConflictSet {
        conflicts: c
            .conflicts
            .iter()
            .map(|p| Conflict {
                path: p.path.as_bytes().to_vec(),
                kind: (match p.kind {
                    K::Content | K::RenameRename => ConflictKind::Content,
                    K::AddAdd => ConflictKind::AddAdd,
                    K::ModifyDelete => ConflictKind::ModifyDelete,
                    K::Symlink => ConflictKind::Symlink,
                    K::Binary => ConflictKind::Binary,
                    K::Redacted => ConflictKind::Redacted,
                    K::Mode => ConflictKind::Mode,
                    K::FileDirectory => ConflictKind::FileDirectory,
                }) as i32,
                base: side(&p.base),
                ours: side(&p.ours),
                theirs: side(&p.theirs),
                hunks: p
                    .hunks
                    .iter()
                    .map(|h| ConflictHunk {
                        base_start: h.base_start as u32,
                        base_len: h.base_len as u32,
                        ours_start: h.ours_start as u32,
                        ours_len: h.ours_len as u32,
                        theirs_start: h.theirs_start as u32,
                        theirs_len: h.theirs_len as u32,
                    })
                    .collect(),
            })
            .collect(),
        clean_entries: Vec::new(),
        base: match &c.base {
            bee_history::MergeBase::Unique(b) => Some(proto_id(b)),
            _ => None,
        },
        ours: ours.cloned(),
        theirs: theirs.cloned(),
    }
}

/// `MERGE_CONFLICT` carrying the conflicts.
pub fn merge_conflict(set: ConflictSet) -> ForgeError {
    let paths: Vec<String> = set
        .conflicts
        .iter()
        .map(|c| String::from_utf8_lossy(&c.path).into_owned())
        .collect();
    ForgeError::new(
        bee_proto::v1::ErrorReason::MergeConflict,
        format!("conflicts in {}", paths.join(", ")),
    )
    .with_payload(bee_proto::v1::error_detail::Payload::Conflicts(set))
}

/// [`ChangeVcs`] and [`Lander`] over the VCS stores through `bee-history`.
pub struct HistoryVcs {
    writer: Writer,
    clock: Arc<dyn Clock>,
}

impl HistoryVcs {
    /// Over `writer`, timing new changesets with `clock`.
    pub fn new(writer: Writer, clock: Arc<dyn Clock>) -> HistoryVcs {
        HistoryVcs { writer, clock }
    }

    /// The VCS org (object namespace) and repo ids of a forge repo. A repo's namespace is
    /// its org, except for forks, which live in their network's namespace
    /// ([`crate::forks::vcs_namespace`]).
    async fn ids(&self, repo: &Repo) -> Result<(OrgId, RepoId)> {
        let id = RepoId(ulid(&repo.id)?);
        let org = crate::forks::vcs_namespace(self.writer.meta().as_ref(), repo).await?;
        Ok((org, id))
    }

    /// The history handle of a repo.
    pub async fn history(&self, repo: &Repo) -> Result<History> {
        let (org, id) = self.ids(repo).await?;
        Ok(History::new(self.writer.clone(), org, id))
    }

    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Points `name` at `target` with a plain compare-and-swap on its current version
    /// (development and tests; protected refs move only through [`Lander`]).
    pub async fn set_ref(&self, repo: &Repo, name: &str, target: &ObjectId) -> Result<u64> {
        let (_, rid) = self.ids(repo).await?;
        let name = RefName::new(name).map_err(|e| ForgeError::invalid("invalid_ref", e.message))?;
        let cur = self
            .writer
            .meta()
            .get_ref(rid, &name)
            .await
            .map_err(vcs_err)?;
        let expected = match cur {
            Some(r) if r.target.is_some() || r.version > 0 => Expected::Version(r.version),
            _ => Expected::Absent,
        };
        let out = self
            .writer
            .update_ref(RefUpdate::set(
                rid,
                name,
                expected,
                Some(core_id(target)?),
                "forge",
                self.now(),
            ))
            .await
            .map_err(vcs_err)?;
        Ok(out.new_version)
    }

    /// Points `name` at `target` with a plain compare-and-swap, only if the ref currently
    /// targets `expected` (`None` = the ref is absent or deleted). Answers `false` without
    /// writing when the ref moved, so callers re-read and decide again. For non-protected refs
    /// the forge maintains itself (`green/*`, SPEC §5).
    pub async fn set_ref_if(
        &self,
        repo: &Repo,
        name: &str,
        expected: Option<&ObjectId>,
        target: &ObjectId,
    ) -> Result<bool> {
        let (_, rid) = self.ids(repo).await?;
        let name = RefName::new(name).map_err(|e| ForgeError::invalid("invalid_ref", e.message))?;
        let cur = self
            .writer
            .meta()
            .get_ref(rid, &name)
            .await
            .map_err(vcs_err)?;
        let cur_target = cur.as_ref().and_then(|r| r.target).map(|t| proto_id(&t));
        if cur_target.as_ref() != expected {
            return Ok(false);
        }
        let want = match cur {
            Some(r) if r.target.is_some() || r.version > 0 => Expected::Version(r.version),
            _ => Expected::Absent,
        };
        match self
            .writer
            .update_ref(RefUpdate::set(
                rid,
                name,
                want,
                Some(core_id(target)?),
                "forge",
                self.now(),
            ))
            .await
            .map_err(vcs_err)
        {
            Ok(_) => Ok(true),
            Err(e) if e.reason == bee_proto::v1::ErrorReason::VersionMismatch => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn upload_meta(&self, repo: &Repo, c: &NewCommit) -> Result<CoreId> {
        let (org, _) = self.ids(repo).await?;
        let mut meta = Meta::new(
            c.author.clone(),
            Timestamp::new((self.now() / 1000) as i64, 0),
            c.creator.clone(),
            c.message.clone(),
        );
        meta.provenance = Some(Provenance {
            principal_chain: c.chain.clone(),
            runtime: None,
            model: None,
            session: None,
            prompt_digest: None,
            checks_run: Vec::new(),
            cost: None,
        });
        let id = meta.id();
        self.writer
            .upload(org, vec![(id, meta.encode())], None, self.now())
            .await
            .map_err(vcs_err)?;
        Ok(id)
    }

    fn derive_change_id(parent: Option<&ObjectId>, c: &NewCommit) -> CoreChangeId {
        let mut h = blake3::Hasher::new();
        h.update(b"swarm-forge-change-id\0");
        if let Some(p) = parent {
            h.update(&p.raw);
        }
        for (path, content) in &c.edits {
            h.update(path.as_bytes());
            h.update(&[0]);
            match content {
                Some(b) => {
                    h.update(blob_id(b).as_bytes());
                }
                None => {
                    h.update(b"-");
                }
            }
        }
        h.update(c.message.as_bytes());
        let mut raw = [0u8; 16];
        raw.copy_from_slice(&h.finalize().as_bytes()[..16]);
        CoreChangeId::new(raw)
    }

    async fn rebase_one(
        &self,
        history: &History,
        c: &CoreId,
        onto: &CoreId,
    ) -> Result<std::result::Result<CoreId, ConflictSet>> {
        match history
            .rebase(c, onto, &RebaseOptions::default(), self.now())
            .await
            .map_err(vcs_err)?
        {
            RebaseOutcome::Unchanged(id) | RebaseOutcome::Rebased(id) => Ok(Ok(id)),
            RebaseOutcome::Conflicts(set) => Ok(Err(conflict_proto(
                &set,
                Some(&proto_id(onto)),
                Some(&proto_id(c)),
            ))),
        }
    }
}

#[async_trait]
impl ChangeVcs for HistoryVcs {
    async fn ref_tip(&self, repo: &Repo, name: &str) -> Result<Option<Ref>> {
        let (_, rid) = self.ids(repo).await?;
        let rn = RefName::new(name).map_err(|e| ForgeError::invalid("invalid_ref", e.message))?;
        let row = self
            .writer
            .meta()
            .get_ref(rid, &rn)
            .await
            .map_err(vcs_err)?;
        Ok(row.and_then(|r| {
            r.target.map(|t| Ref {
                name: name.to_string(),
                target: Some(proto_id(&t)),
                version: r.version,
            })
        }))
    }

    async fn stack(&self, repo: &Repo, head: &ObjectId, onto: &ObjectId) -> Result<StackInfo> {
        let h = self.history(repo).await?;
        let head_id = core_id(head)?;
        let onto_id = core_id(onto)?;
        let mut members = Vec::new();
        let mut change_ids = Vec::new();
        let mut has_merges = false;
        let mut x = head_id;
        let base;
        loop {
            if h.is_ancestor(&x, &onto_id).await.map_err(vcs_err)? {
                base = x;
                break;
            }
            if members.len() >= MAX_STACK {
                return Err(ForgeError::too_large(
                    "stack_changesets",
                    MAX_STACK as u64,
                    members.len() as u64 + 1,
                ));
            }
            let cs = h.changeset(&x).await.map_err(vcs_err)?;
            has_merges |= cs.parents().len() > 1;
            members.push(proto_id(&x));
            change_ids.push(proto_change(cs.change_id()));
            match cs.first_parent() {
                Some(p) => x = *p,
                None => {
                    return Err(ForgeError::invalid(
                        "unrelated_history",
                        "the head shares no history with the target",
                    ));
                }
            }
        }
        if members.is_empty() {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::EmptyChange,
                "the head is already part of the target",
            ));
        }
        members.reverse();
        change_ids.reverse();
        Ok(StackInfo {
            members,
            base: proto_id(&base),
            change_ids,
            has_merges,
        })
    }

    async fn commit(&self, repo: &Repo, id: &ObjectId) -> Result<CommitInfo> {
        let h = self.history(repo).await?;
        let cs = h.changeset(&core_id(id)?).await.map_err(vcs_err)?;
        let meta = h.meta_object(cs.meta()).await.map_err(vcs_err)?;
        Ok(CommitInfo {
            id: id.clone(),
            parents: cs.parents().iter().map(proto_id).collect(),
            change_id: proto_change(cs.change_id()),
            message: meta.as_ref().map(|m| m.message.clone()).unwrap_or_default(),
            author: meta.map(|m| m.author).unwrap_or_default(),
        })
    }

    async fn classify(
        &self,
        repo: &Repo,
        members: &[ObjectId],
        rules: &ClassRules,
        fingerprint: &str,
    ) -> Result<Classified> {
        let h = Arc::new(self.history(repo).await?);
        let mut reg = DerivedRegistry::standard();
        reg.register(Arc::new(DiffClassType::with_rules(
            rules.clone(),
            fingerprint.to_string(),
        )));
        let derived = Derived::new(h.clone(), Arc::new(reg));
        let mut per: BTreeMap<String, bee_classify::Class> = BTreeMap::new();
        for m in members {
            let v = derived.diff_class(&core_id(m)?).await.map_err(vcs_err)?;
            for (p, c, _) in v.per_path {
                let key = String::from_utf8_lossy(p.as_bytes()).into_owned();
                let e = per.entry(key).or_insert(c);
                if c > *e {
                    *e = c;
                }
            }
        }
        let root = match members.last() {
            Some(head) => Some(*h.changeset(&core_id(head)?).await.map_err(vcs_err)?.root()),
            None => None,
        };
        let mut paths = Vec::with_capacity(per.len());
        let mut highest: Option<bee_classify::Class> = None;
        for (path, class) in per {
            let size = match &root {
                Some(r) => h
                    .entry_at(r, &repo_path(&path)?)
                    .await
                    .map_err(vcs_err)?
                    .filter(|e| e.is_leaf())
                    .map(|e| e.size),
                None => None,
            };
            highest = Some(highest.map_or(class, |h| h.max(class)));
            paths.push(ClassifiedPath {
                path,
                class: class.as_str().to_string(),
                size,
            });
        }
        Ok(Classified {
            highest: highest.map(|c| c.as_str().to_string()).unwrap_or_default(),
            paths,
        })
    }

    async fn diff(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
        paths: &[String],
        hunks: bool,
        context: u32,
    ) -> Result<Vec<DiffItem>> {
        self.diff_with(repo, from, to, paths, hunks, context, false)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn diff_with(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
        paths: &[String],
        hunks: bool,
        context: u32,
        ignore_whitespace: bool,
    ) -> Result<Vec<DiffItem>> {
        let h = self.history(repo).await?;
        let opts = DiffOptions {
            paths: paths
                .iter()
                .map(|p| repo_path(p))
                .collect::<Result<Vec<_>>>()?,
            // Whitespace-only files are recognised by their hunks.
            hunks: hunks || ignore_whitespace,
            context: if context == 0 { 3 } else { context as usize },
            detect_renames: true,
            ignore_whitespace,
        };
        let files = h
            .diff(Some(&core_id(from)?), &core_id(to)?, &opts)
            .await
            .map_err(vcs_err)?;
        let files: Vec<_> = files
            .into_iter()
            .filter(|f| {
                !(ignore_whitespace
                    && !f.binary
                    && matches!(f.change, FileChange::Modified)
                    && f.old.zip(f.new).is_some_and(|(o, n)| o.id != n.id)
                    && f.hunks.is_empty())
            })
            .map(|mut f| {
                if !hunks {
                    f.hunks.clear();
                }
                f
            })
            .collect();
        Ok(files
            .into_iter()
            .map(|f| {
                let (status, old_path, similarity) = match &f.change {
                    FileChange::Added => (DiffStatus::Added, Vec::new(), 0),
                    FileChange::Deleted => (DiffStatus::Deleted, Vec::new(), 0),
                    FileChange::Modified => {
                        let mode_only = f.old.zip(f.new).is_some_and(|(o, n)| o.id == n.id);
                        (
                            if mode_only {
                                DiffStatus::ModeChanged
                            } else {
                                DiffStatus::Modified
                            },
                            Vec::new(),
                            0,
                        )
                    }
                    FileChange::Renamed { from, similarity } => (
                        DiffStatus::Renamed,
                        from.as_bytes().to_vec(),
                        similarity.map_or(100, |(s, u)| {
                            (s * 100).checked_div(u).map_or(100, |v| v as u32)
                        }),
                    ),
                };
                DiffItem {
                    path: f.path.as_bytes().to_vec(),
                    old: f.old.map(|e| ConflictSide {
                        mode: proto_mode(e.mode),
                        blob: Some(proto_id(&e.id)),
                    }),
                    new: f.new.map(|e| ConflictSide {
                        mode: proto_mode(e.mode),
                        blob: Some(proto_id(&e.id)),
                    }),
                    binary: f.binary,
                    hunks: f
                        .hunks
                        .iter()
                        .map(|hk| DiffHunk {
                            old_start: hk.old_start as u32 + 1,
                            old_len: hk.old_len as u32,
                            new_start: hk.new_start as u32 + 1,
                            new_len: hk.new_len as u32,
                            lines: hk
                                .lines
                                .iter()
                                .map(|l| {
                                    let p = match l.kind {
                                        bee_history::LineKind::Context => ' ',
                                        bee_history::LineKind::Added => '+',
                                        bee_history::LineKind::Removed => '-',
                                    };
                                    let text = String::from_utf8_lossy(&l.content);
                                    format!("{p}{}", text.trim_end_matches('\n'))
                                })
                                .collect(),
                        })
                        .collect(),
                    diff_class: String::new(),
                    status: status as i32,
                    old_path,
                    similarity,
                    additions: f.added as u32,
                    deletions: f.removed as u32,
                    truncated: false,
                }
            })
            .collect())
    }

    async fn read_file(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<Vec<u8>>> {
        let h = self.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        let Some(e) = h
            .entry_at(&root, &repo_path(path)?)
            .await
            .map_err(vcs_err)?
        else {
            return Ok(None);
        };
        if !e.is_leaf() || e.mode == Mode::Gitlink {
            return Ok(None);
        }
        match h.blob(&e.id).await.map_err(vcs_err)? {
            bee_history::BlobContent::Bytes(b) => Ok(Some(b)),
            bee_history::BlobContent::Redacted => Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Redacted,
                format!("{path} is redacted"),
            )),
        }
    }

    async fn file_id(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<ObjectId>> {
        let h = self.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        Ok(h.entry_at(&root, &repo_path(path)?)
            .await
            .map_err(vcs_err)?
            .filter(|e| e.is_leaf())
            .map(|e| proto_id(&e.id)))
    }

    async fn list_dir(&self, repo: &Repo, at: &ObjectId, dir: &str) -> Result<Vec<String>> {
        let h = self.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        let tree = if dir.is_empty() {
            root
        } else {
            match h.entry_at(&root, &repo_path(dir)?).await.map_err(vcs_err)? {
                Some(e) if e.mode == Mode::Tree => e.id,
                _ => return Ok(Vec::new()),
            }
        };
        let t = h.tree(&tree).await.map_err(vcs_err)?;
        Ok(t.entries()
            .iter()
            .filter(|e| e.mode != Mode::Tree)
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect())
    }

    async fn create_commit(&self, repo: &Repo, c: NewCommit) -> Result<ObjectId> {
        let (org, rid) = self.ids(repo).await?;
        let h = self.history(repo).await?;
        let mut blobs = BTreeMap::new();
        let mut delta = Vec::with_capacity(c.edits.len());
        let parent_root = match &c.parent {
            Some(p) => Some(*h.changeset(&core_id(p)?).await.map_err(vcs_err)?.root()),
            None => None,
        };
        // Deltas are sorted by path with one entry per path (the last edit wins).
        let mut sorted: BTreeMap<RepoPath, &Option<Vec<u8>>> = BTreeMap::new();
        for (path, content) in &c.edits {
            sorted.insert(repo_path(path)?, content);
        }
        for (rp, content) in sorted {
            match content {
                Some(bytes) => {
                    let id = blob_id(bytes);
                    let mode = match &parent_root {
                        Some(r) => h
                            .entry_at(r, &rp)
                            .await
                            .map_err(vcs_err)?
                            .filter(|e| e.is_leaf() && e.mode != Mode::Gitlink)
                            .map_or(Mode::File, |e| e.mode),
                        None => Mode::File,
                    };
                    blobs.insert(id, bytes.clone());
                    delta.push(DeltaEntry::put(rp, mode, id, bytes.len() as u64));
                }
                None => delta.push(DeltaEntry::delete(rp)),
            }
        }
        if !blobs.is_empty() {
            self.writer
                .upload(
                    org,
                    blobs.into_iter().collect(),
                    Some(UploadRecord {
                        repo: Some(rid),
                        principal: c.creator.clone(),
                    }),
                    self.now(),
                )
                .await
                .map_err(vcs_err)?;
        }
        let meta = self.upload_meta(repo, &c).await?;
        let change_id = match &c.change_id {
            Some(raw) => CoreChangeId::from_bytes(raw)
                .map_err(|e| ForgeError::invalid("invalid_change_id", e.message))?,
            None => Self::derive_change_id(c.parent.as_ref(), &c),
        };
        let parents = match &c.parent {
            Some(p) => vec![core_id(p)?],
            None => Vec::new(),
        };
        let id = h
            .create_changeset(parents, meta, change_id, delta, self.now())
            .await
            .map_err(vcs_err)?;
        Ok(proto_id(&id))
    }

    async fn rebase_stack(
        &self,
        repo: &Repo,
        members: &[ObjectId],
        onto: &ObjectId,
    ) -> Result<Rebased> {
        let h = self.history(repo).await?;
        let ids = members.iter().map(core_id).collect::<Result<Vec<_>>>()?;
        let onto_id = core_id(onto)?;
        match h
            .rebase_stack(&ids, &onto_id, &RebaseOptions::default(), self.now())
            .await
            .map_err(vcs_err)?
        {
            StackOutcome::Rebased(steps) => Ok(Rebased::Head(
                steps
                    .last()
                    .map(|s| proto_id(&s.result))
                    .unwrap_or_else(|| onto.clone()),
            )),
            StackOutcome::Conflict {
                source, conflicts, ..
            } => Ok(Rebased::Conflict(conflict_proto(
                &conflicts,
                Some(onto),
                Some(&proto_id(&source)),
            ))),
        }
    }

    async fn revert(
        &self,
        repo: &Repo,
        restore: &ObjectId,
        landed: &ObjectId,
        onto: &ObjectId,
        commit: NewCommit,
    ) -> Result<Rebased> {
        let h = self.history(repo).await?;
        let restore_root = *h
            .changeset(&core_id(restore)?)
            .await
            .map_err(vcs_err)?
            .root();
        let landed_id = core_id(landed)?;
        let landed_root = *h.changeset(&landed_id).await.map_err(vcs_err)?.root();
        let delta = h
            .diff_trees(Some(&landed_root), Some(&restore_root), None)
            .await
            .map_err(vcs_err)?;
        if delta.is_empty() {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::EmptyChange,
                "nothing to revert",
            ));
        }
        let meta = self.upload_meta(repo, &commit).await?;
        let change_id = match &commit.change_id {
            Some(raw) => CoreChangeId::from_bytes(raw)
                .map_err(|e| ForgeError::invalid("invalid_change_id", e.message))?,
            None => Self::derive_change_id(Some(landed), &commit),
        };
        let revert = h
            .create_changeset(vec![landed_id], meta, change_id, delta, self.now())
            .await
            .map_err(vcs_err)?;
        let onto_id = core_id(onto)?;
        if onto_id == landed_id {
            return Ok(Rebased::Head(proto_id(&revert)));
        }
        Ok(match self.rebase_one(&h, &revert, &onto_id).await? {
            Ok(id) => Rebased::Head(proto_id(&id)),
            Err(set) => Rebased::Conflict(set),
        })
    }

    async fn is_ancestor(&self, repo: &Repo, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        let h = self.history(repo).await?;
        h.is_ancestor(&core_id(a)?, &core_id(b)?)
            .await
            .map_err(vcs_err)
    }

    async fn merge(
        &self,
        repo: &Repo,
        head: &ObjectId,
        onto: &ObjectId,
        commit: NewCommit,
    ) -> Result<Rebased> {
        let h = self.history(repo).await?;
        let meta = self.upload_meta(repo, &commit).await?;
        let cid = match &commit.change_id {
            Some(raw) => CoreChangeId::from_bytes(raw)
                .map_err(|e| ForgeError::invalid("invalid_change_id", e.message))?,
            None => Self::derive_change_id(Some(head), &commit),
        };
        match h
            .merge(
                &core_id(head)?,
                &core_id(onto)?,
                meta,
                cid,
                &MergeOptions::default(),
                self.now(),
            )
            .await
            .map_err(vcs_err)?
        {
            MergeOutcome::Merged { id, .. } => Ok(Rebased::Head(proto_id(&id))),
            MergeOutcome::Conflicts(set) => Ok(Rebased::Conflict(conflict_proto(
                &set,
                Some(head),
                Some(onto),
            ))),
        }
    }
}

#[async_trait]
impl Lander for HistoryVcs {
    async fn land(&self, repo: &Repo, intent: &LandIntent) -> Result<LandResponse> {
        let (_, rid) = self.ids(repo).await?;
        let h = self.history(repo).await?;
        let name = RefName::new(intent.ref_name.as_str())
            .map_err(|e| ForgeError::invalid("invalid_ref", e.message))?;
        let head = core_id(&intent.head)?;
        let principal = intent.chain.first().cloned().unwrap_or_default();
        for attempt in 1..=LAND_ATTEMPTS {
            let Some(tip_row) = self
                .writer
                .meta()
                .get_ref(rid, &name)
                .await
                .map_err(vcs_err)?
                .filter(|r| r.target.is_some())
            else {
                return Err(ForgeError::not_found(format!("ref {}", intent.ref_name)));
            };
            if intent.expected_version != 0 && intent.expected_version != tip_row.version {
                return Err(ForgeError::version_mismatch(
                    intent.expected_version,
                    tip_row.version,
                ));
            }
            let tip = tip_row.target.unwrap_or(head);
            // Stack: first-parent chain back to the first ancestor of the tip.
            let mut members = Vec::new();
            let mut x = head;
            while !h.is_ancestor(&x, &tip).await.map_err(vcs_err)? {
                let cs = h.changeset(&x).await.map_err(vcs_err)?;
                if cs.parents().len() > 1 && intent.method != LandMethod::MergeCommit {
                    return Err(ForgeError::new(
                        bee_proto::v1::ErrorReason::MergeInStack,
                        format!("{x} is a merge; this land method cannot take merges"),
                    ));
                }
                members.push((x, *cs.change_id()));
                match cs.first_parent() {
                    Some(p) => x = *p,
                    None => break,
                }
                if members.len() > MAX_STACK {
                    return Err(ForgeError::too_large(
                        "stack_changesets",
                        MAX_STACK as u64,
                        members.len() as u64,
                    ));
                }
            }
            members.reverse();
            let mut todo = Vec::new();
            let mut skipped = Vec::new();
            let cids: Vec<CoreChangeId> = members.iter().map(|m| m.1).collect();
            let rows = self
                .writer
                .meta()
                .get_landed(rid, &name, &cids)
                .await
                .map_err(vcs_err)?;
            for ((src, cid), row) in members.into_iter().zip(rows) {
                match row {
                    Some(r) if r.source == src => skipped.push(LandedChange {
                        change_id: Some(proto_change(&cid)),
                        source: Some(proto_id(&src)),
                        result: Some(proto_id(&r.result)),
                        skipped: true,
                    }),
                    Some(_) => {
                        return Err(ForgeError::new(
                            bee_proto::v1::ErrorReason::AlreadyLanded,
                            format!("change {} landed from another source", cid.to_text()),
                        ));
                    }
                    None => todo.push((src, cid)),
                }
            }
            if todo.is_empty() {
                return Ok(LandResponse {
                    r#ref: Some(Ref {
                        name: intent.ref_name.clone(),
                        target: Some(proto_id(&tip)),
                        version: tip_row.version,
                    }),
                    landed: skipped,
                    attempts: attempt,
                    event_seq: 0,
                });
            }
            let (new_tip, entries): (CoreId, Vec<LandedEntry>) = match intent.method {
                LandMethod::Unspecified | LandMethod::Rebase => {
                    let srcs: Vec<CoreId> = todo.iter().map(|t| t.0).collect();
                    match h
                        .rebase_stack(&srcs, &tip, &RebaseOptions::default(), self.now())
                        .await
                        .map_err(vcs_err)?
                    {
                        StackOutcome::Rebased(steps) => (
                            steps.last().map_or(tip, |s| s.result),
                            steps
                                .iter()
                                .map(|s| LandedEntry {
                                    change_id: s.change_id,
                                    source: s.source,
                                    result: s.result,
                                })
                                .collect(),
                        ),
                        StackOutcome::Conflict {
                            source, conflicts, ..
                        } => {
                            return Err(merge_conflict(conflict_proto(
                                &conflicts,
                                Some(&proto_id(&tip)),
                                Some(&proto_id(&source)),
                            )));
                        }
                    }
                }
                LandMethod::Squash => {
                    let first = todo[0].0;
                    let base = *h
                        .changeset(&first)
                        .await
                        .map_err(vcs_err)?
                        .first_parent()
                        .unwrap_or(&tip);
                    let base_root = *h.changeset(&base).await.map_err(vcs_err)?.root();
                    let head_root = *h.changeset(&head).await.map_err(vcs_err)?.root();
                    let delta = h
                        .diff_trees(Some(&base_root), Some(&head_root), None)
                        .await
                        .map_err(vcs_err)?;
                    let commit = NewCommit {
                        message: intent.message.clone(),
                        author: principal.clone(),
                        creator: principal.clone(),
                        chain: intent.chain.clone(),
                        ..NewCommit::default()
                    };
                    let meta = self.upload_meta(repo, &commit).await?;
                    let cid = match &intent.change_id {
                        Some(raw) => CoreChangeId::from_bytes(raw)
                            .map_err(|e| ForgeError::invalid("invalid_change_id", e.message))?,
                        None => todo[0].1,
                    };
                    let squashed = h
                        .create_changeset(vec![base], meta, cid, delta, self.now())
                        .await
                        .map_err(vcs_err)?;
                    let result = match self.rebase_one(&h, &squashed, &tip).await? {
                        Ok(id) => id,
                        Err(set) => return Err(merge_conflict(set)),
                    };
                    (
                        result,
                        todo.iter()
                            .map(|(src, c)| LandedEntry {
                                change_id: *c,
                                source: *src,
                                result,
                            })
                            .collect(),
                    )
                }
                LandMethod::MergeCommit => {
                    let commit = NewCommit {
                        message: intent.message.clone(),
                        author: principal.clone(),
                        creator: principal.clone(),
                        chain: intent.chain.clone(),
                        ..NewCommit::default()
                    };
                    let meta = self.upload_meta(repo, &commit).await?;
                    let cid = match &intent.change_id {
                        Some(raw) => CoreChangeId::from_bytes(raw)
                            .map_err(|e| ForgeError::invalid("invalid_change_id", e.message))?,
                        None => Self::derive_change_id(Some(&intent.head), &commit),
                    };
                    match h
                        .merge(&tip, &head, meta, cid, &MergeOptions::default(), self.now())
                        .await
                        .map_err(vcs_err)?
                    {
                        MergeOutcome::Merged { id, .. } => (
                            id,
                            todo.iter()
                                .map(|(src, c)| LandedEntry {
                                    change_id: *c,
                                    source: *src,
                                    result: *src,
                                })
                                .collect(),
                        ),
                        MergeOutcome::Conflicts(set) => {
                            return Err(merge_conflict(conflict_proto(
                                &set,
                                Some(&proto_id(&tip)),
                                Some(&intent.head),
                            )));
                        }
                    }
                }
            };
            let update = RefUpdate {
                repo: rid,
                name: name.clone(),
                expected: Expected::Version(tip_row.version),
                steps: vec![RefStep {
                    target: Some(new_tip),
                    landed: entries.clone(),
                }],
                principal: principal.clone(),
                at_ms: self.now(),
                events: Vec::new(),
            };
            match self.writer.update_ref(update).await {
                Ok(out) => {
                    let mut landed = skipped;
                    landed.extend(entries.iter().map(|e| LandedChange {
                        change_id: Some(proto_change(&e.change_id)),
                        source: Some(proto_id(&e.source)),
                        result: Some(proto_id(&e.result)),
                        skipped: false,
                    }));
                    return Ok(LandResponse {
                        r#ref: Some(Ref {
                            name: intent.ref_name.clone(),
                            target: Some(proto_id(&new_tip)),
                            version: out.new_version,
                        }),
                        landed,
                        attempts: attempt,
                        event_seq: out.event_seqs.first().copied().unwrap_or(0),
                    });
                }
                Err(e) if e.reason == bee_core::Reason::VersionMismatch => continue,
                Err(e) => return Err(vcs_err(e)),
            }
        }
        Err(ForgeError::new(
            bee_proto::v1::ErrorReason::LandContention,
            "the land attempt budget ran out",
        ))
    }
}
