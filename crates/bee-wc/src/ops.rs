//! Working-copy operations. Each does its server work first (deterministic and safe to
//! retry), then commits the new local state through the journal in one step.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use bee_core::{ChangeId, Mode, ObjectId};
use bee_proto::v1;

use crate::checkout;
use crate::conflict::{self, SideBytes};
use crate::error::{Result, WcError};
use crate::fsutil::{self, fs_path, path_text};
use crate::journal::{self, OpRecord};
use crate::manifest::{self, Change, Entry, Manifest};
use crate::remote::{
    NewChangeset, Remote, Signature, Transport, from_delta_entry, from_pid, from_pmode,
};
use crate::report::{LandView, LandedView, OpReport, subject};
use crate::scan::read_entry_with;
use crate::sparse::Sparse;
use crate::state::{
    self, Config, ConflictRecord, PathBytes, RebaseKind, RebaseOp, StackEntry, State,
};
use crate::wc::{BEE_DIR, Commit, OpenOptions, WorkingCopy, targets_between};

/// How to make a new working copy.
#[derive(Clone, Debug)]
pub struct CreateOptions {
    /// Endpoint to remember in the config.
    pub endpoint: Option<String>,
    /// Repo, `org/name` or an id.
    pub repo: String,
    /// Protected ref to sync with and land on.
    pub target_ref: String,
    /// Sparse profile.
    pub sparse: Sparse,
    /// Seed for ChangeIds; random when `None`.
    pub seed: Option<[u8; 32]>,
    /// `init`: adopt a non-empty directory, keeping files already there.
    pub adopt: bool,
}

/// What to commit.
#[derive(Clone, Debug)]
pub struct CommitOptions {
    /// Message.
    pub message: String,
    /// Author and time.
    pub author: Signature,
    /// Commit even with no changes.
    pub allow_empty: bool,
    /// Only paths under these repo-relative prefixes (empty = everything).
    pub paths: Vec<String>,
    /// Provenance claims (runtime, model, session).
    pub provenance: Option<v1::Provenance>,
}

/// Which side `resolve` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// The target side.
    Ours,
    /// The moved change's side.
    Theirs,
    /// The merge base.
    Base,
}

/// A file's diff against the base.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    /// Repo path.
    pub path: String,
    /// `added`, `modified` or `deleted`.
    pub status: String,
    /// Old mode (`file`, `executable`, `symlink`), if present before.
    pub old_mode: Option<String>,
    /// New mode, if present after.
    pub new_mode: Option<String>,
    /// Either side is binary.
    pub binary: bool,
    /// Lines added.
    pub additions: usize,
    /// Lines removed.
    pub deletions: usize,
    /// Unified diff text (empty for binary files).
    pub unified: String,
}

enum Drive {
    Done(RebaseOp),
    Stopped(RebaseOp),
}

fn mode_name(m: Mode) -> String {
    match m {
        Mode::File => "file",
        Mode::Executable => "executable",
        Mode::Symlink => "symlink",
        Mode::Tree => "tree",
        Mode::Gitlink => "gitlink",
    }
    .to_owned()
}

fn under_any(path: &[u8], prefixes: &[String]) -> bool {
    prefixes.is_empty()
        || prefixes.iter().any(|p| {
            let p = p.trim_matches('/');
            p.is_empty()
                || p == "."
                || path == p.as_bytes()
                || (path.len() > p.len() && path.starts_with(p.as_bytes()) && path[p.len()] == b'/')
        })
}

/// Composes delta `d2` (relative to `parent + d1`) onto `d1` (relative to `parent`), given the
/// parent's entries for paths both touch: a path that ends where it started drops out.
pub fn compose(
    d1: &[(Vec<u8>, Option<Entry>)],
    d2: &[(Vec<u8>, Option<Entry>)],
    parent: &BTreeMap<Vec<u8>, Option<Entry>>,
) -> Vec<(Vec<u8>, Option<Entry>)> {
    let first: BTreeMap<&Vec<u8>, &Option<Entry>> = d1.iter().map(|(p, e)| (p, e)).collect();
    let mut out: BTreeMap<Vec<u8>, Option<Entry>> = d1.iter().cloned().collect();
    for (p, e) in d2 {
        out.insert(p.clone(), *e);
        if first.contains_key(p) && parent.get(p).copied().flatten() == *e {
            out.remove(p);
        }
    }
    out.into_iter().collect()
}

/// Paths touched by both deltas.
fn overlap(d1: &[(Vec<u8>, Option<Entry>)], d2: &[(Vec<u8>, Option<Entry>)]) -> Vec<Vec<u8>> {
    let a: BTreeSet<&Vec<u8>> = d1.iter().map(|(p, _)| p).collect();
    d2.iter()
        .filter(|(p, _)| a.contains(p))
        .map(|(p, _)| p.clone())
        .collect()
}

fn delta_of(cs: &v1::Changeset) -> Result<Vec<(Vec<u8>, Option<Entry>)>> {
    cs.delta.iter().map(from_delta_entry).collect()
}

fn signature_of(meta: Option<&v1::Meta>, fallback: &Signature) -> Signature {
    match meta {
        Some(m) => Signature {
            name: if m.author.is_empty() {
                fallback.name.clone()
            } else {
                m.author.clone()
            },
            time: m
                .author_time
                .as_ref()
                .and_then(|t| t.time.as_ref())
                .map_or(fallback.time, |t| t.seconds),
            tz_offset_minutes: m
                .author_time
                .as_ref()
                .map_or(fallback.tz_offset_minutes, |t| t.tz_offset_minutes),
        },
        None => fallback.clone(),
    }
}

fn random_seed() -> Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| WcError::internal("no_entropy", format!("cannot read random bytes: {e}")))?;
    Ok(seed)
}

/// Makes a new working copy in `dir` (`bee clone`, or `bee init` with `adopt`).
pub fn create(
    dir: &Path,
    opts: &CreateOptions,
    t: &dyn Transport,
    open: &OpenOptions,
) -> Result<(WorkingCopy, OpReport)> {
    let bee = dir.join(BEE_DIR);
    if bee.join("config.json").exists() {
        return Err(WcError::state(
            "already_a_working_copy",
            format!("{} is already a bee working copy", dir.display()),
        ));
    }
    if dir.exists() && !opts.adopt {
        let mut entries = fs::read_dir(dir)
            .map_err(|e| WcError::io(format!("read {}", dir.display()), &e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != BEE_DIR);
        if entries.next().is_some() {
            return Err(WcError::usage(
                "directory_not_empty",
                format!(
                    "{} is not empty; clone into a new directory, or use `bee init` to adopt it",
                    dir.display()
                ),
            ));
        }
    }
    bee_core::RefName::new(opts.target_ref.as_str()).map_err(|e| {
        WcError::usage(
            "invalid_ref_name",
            format!("{}: {}", opts.target_ref, e.message),
        )
    })?;
    let remote = Remote::new(t, &opts.repo);
    let (tip, version) = remote.resolve_ref(&opts.target_ref, 0)?;
    let (manifest, _restricted) = remote.manifest(&tip, &opts.sparse)?;
    fs::create_dir_all(dir).map_err(|e| WcError::io(format!("create {}", dir.display()), &e))?;
    if bee.exists() {
        // A creation that crashed before its config was written left no working copy.
        fs::remove_dir_all(&bee)
            .map_err(|e| WcError::io(format!("remove {}", bee.display()), &e))?;
    }
    let seed = match opts.seed {
        Some(s) => s,
        None => random_seed()?,
    };
    let config = Config {
        version: 1,
        endpoint: opts.endpoint.clone(),
        repo: opts.repo.clone(),
        target_ref: opts.target_ref.clone(),
        seed: bee_core::hex::encode(&seed),
    };
    let state = State {
        version: 1,
        head: tip,
        onto: tip,
        onto_version: version,
        stack: Vec::new(),
        sparse: opts.sparse.clone(),
        rebase: None,
        hidden: Vec::new(),
        next_change: 0,
        redaction_epoch: 0,
    };
    let mut wc = WorkingCopy::create_locked(dir, config, state.clone(), open)?;
    let mut targets = Vec::new();
    for (p, e) in &manifest {
        if opts.adopt {
            let path = fs_path(dir, p)?;
            if fs::symlink_metadata(&path).is_ok() {
                continue;
            }
        }
        targets.push((PathBytes(p.clone()), Some(*e)));
    }
    let files = manifest.len();
    let op = if opts.adopt { "init" } else { "clone" };
    let mut report = wc.commit_local(
        t,
        Commit {
            op,
            summary: format!("{op} {}@{} ({files} files)", opts.repo, opts.target_ref),
            state,
            manifest,
            targets,
            soft: false,
            undoable: false,
            undoes: None,
        },
    )?;
    report.summary = format!(
        "{op} {} at {} version {version}",
        opts.repo, opts.target_ref
    );
    report.changeset = Some(tip.to_text());
    Ok((wc, report))
}

impl WorkingCopy {
    fn refuse_rebase(&self, what: &str) -> Result<()> {
        if self.state.rebase.is_some() {
            return Err(WcError::state(
                "rebase_in_progress",
                format!(
                    "{what} refused: a rebase is in progress; resolve the conflicts and run \
                     `bee resolve`, or `bee sync --abort`"
                ),
            ));
        }
        Ok(())
    }

    /// The stack position of head: `Some(i)` for a stack entry, `None` at the bottom.
    fn head_pos(&self) -> Option<usize> {
        self.state.head_index()
    }

    /// Reads changed files from disk, uploads their blobs and returns the delta.
    fn upload_changes(
        &self,
        t: &dyn Transport,
        changes: &[Change],
    ) -> Result<Vec<(Vec<u8>, Option<Entry>)>> {
        let mut delta = Vec::with_capacity(changes.len());
        let mut blobs = Vec::new();
        let extra: Vec<Vec<u8>> = changes.iter().map(|c| c.path.clone()).collect();
        let attrs = self.worktree_attributes(&extra)?;
        for c in changes {
            match &c.new {
                None => delta.push((c.path.clone(), None)),
                Some(_) => {
                    let path = fs_path(&self.root, &c.path)?;
                    let meta = fs::symlink_metadata(&path)
                        .map_err(|e| WcError::io(format!("stat {}", path.display()), &e))?;
                    let (entry, bytes) = read_entry_with(&path, &meta, &c.path, &attrs)?;
                    if c.old == Some(entry) {
                        continue;
                    }
                    blobs.push((entry.id, bytes));
                    delta.push((c.path.clone(), Some(entry)));
                }
            }
        }
        blobs.sort_by_key(|b| b.0);
        blobs.dedup_by(|a, b| a.0 == b.0);
        self.remote(t).upload(&blobs)?;
        Ok(delta)
    }

    fn entry_at(&self, i: usize) -> Result<StackEntry> {
        self.state.stack.get(i).cloned().ok_or_else(|| {
            WcError::usage(
                "no_such_change",
                format!("the stack has no change {}", i + 1),
            )
        })
    }

    /// Finds a stack change by ChangeId (or unique prefix of its hex), changeset id prefix,
    /// or 1-based position.
    pub fn find_change(&self, spec: &str) -> Result<usize> {
        let spec = spec.trim();
        if let Ok(n) = spec.parse::<usize>()
            && n >= 1
            && n <= self.state.stack.len()
        {
            return Ok(n - 1);
        }
        let matches: Vec<usize> = self
            .state
            .stack
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                spec.len() >= 4
                    && (e.change_id.to_text().starts_with(spec)
                        || e.changeset.to_text().starts_with(spec)
                        || e.change_id.to_text()[2..].starts_with(spec))
            })
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [i] => Ok(*i),
            [] => Err(WcError::usage(
                "no_such_change",
                format!("{spec:?} names no change of the stack (see `bee stack list`)"),
            )),
            _ => Err(WcError::usage(
                "ambiguous_change",
                format!("{spec:?} names several changes of the stack"),
            )),
        }
    }

    /// Moves `pending` onto `base` with server-side rebases, stopping at the first conflict.
    fn drive(&self, remote: &Remote<'_>, mut op: RebaseOp) -> Result<Drive> {
        while !op.pending.is_empty() {
            let entry = op.pending[0].clone();
            match remote.rebase(&entry.changeset, &op.base) {
                Ok(cs) => {
                    op.pending.remove(0);
                    let id = from_pid(cs.id.as_ref(), "changeset id")?;
                    op.done.push(StackEntry {
                        change_id: entry.change_id,
                        changeset: id,
                        parent: op.base,
                        message: entry.message,
                    });
                    op.base = id;
                }
                Err(e) if e.is_remote_reason("MERGE_CONFLICT") => {
                    let set = match e.detail.as_ref().and_then(|d| d.payload.clone()) {
                        Some(v1::error_detail::Payload::Conflicts(set)) => set,
                        _ => {
                            return Err(WcError::internal(
                                "bad_response",
                                "MERGE_CONFLICT without a conflict set",
                            ));
                        }
                    };
                    op.pending.remove(0);
                    op.current = Some(entry);
                    op.conflicts = Vec::new();
                    for c in &set.conflicts {
                        op.conflicts.push(ConflictRecord {
                            path: PathBytes(c.path.clone()),
                            kind: conflict::kind_name(c.kind).to_owned(),
                            base: side(c.base.as_ref())?,
                            ours: side(c.ours.as_ref())?,
                            theirs: side(c.theirs.as_ref())?,
                            rendered: None,
                            markers: false,
                            resolved: false,
                        });
                    }
                    op.clean = set
                        .clean_entries
                        .iter()
                        .map(|d| from_delta_entry(d).map(|(p, e)| (PathBytes(p), e)))
                        .collect::<Result<_>>()?;
                    return Ok(Drive::Stopped(op));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Drive::Done(op))
    }

    /// Commits the outcome of a rebase: a finished stack, or a conflict to resolve.
    ///
    /// `from` is what the working tree holds now; `known` is a manifest already known for a
    /// changeset (saves a listing when the final head is that changeset).
    #[allow(clippy::too_many_arguments)]
    fn settle(
        &mut self,
        t: &dyn Transport,
        drive: Drive,
        op_name: &'static str,
        summary: String,
        from: &Manifest,
        known: Option<(ObjectId, &Manifest)>,
        soft: bool,
    ) -> Result<OpReport> {
        let remote = self.remote(t);
        match drive {
            Drive::Done(op) => {
                let mut st = (*op.abort).clone();
                st.next_change = st.next_change.max(self.state.next_change);
                st.stack = op.kept.iter().chain(op.done.iter()).cloned().collect();
                st.onto = op.new_onto;
                st.onto_version = op.new_onto_version;
                st.rebase = None;
                let returning = op
                    .return_to
                    .and_then(|c| st.stack.iter().find(|e| e.change_id == c));
                st.head = match returning {
                    _ if op.return_bottom => op.new_onto,
                    Some(e) => e.changeset,
                    None if op.return_to.is_none() => {
                        st.stack.last().map_or(op.new_onto, |e| e.changeset)
                    }
                    None => op.new_onto,
                };
                let manifest = match known {
                    Some((id, m)) if id == st.head => m.clone(),
                    _ => remote.manifest(&st.head, &st.sparse)?.0,
                };
                let targets = targets_between(from, &manifest);
                let dropped = op.dropped.clone();
                let mut report = self.commit_local(
                    t,
                    Commit {
                        op: op_name,
                        summary: summary.clone(),
                        state: st,
                        manifest,
                        targets,
                        soft,
                        undoable: true,
                        undoes: None,
                    },
                )?;
                report.summary = summary;
                report.dropped = dropped;
                Ok(report)
            }
            Drive::Stopped(mut op) => {
                let outside: Vec<String> = op
                    .conflicts
                    .iter()
                    .map(|c| &c.path.0)
                    .chain(op.clean.iter().map(|(p, _)| &p.0))
                    .filter(|p| !self.state.sparse.contains(p))
                    .map(|p| path_text(p))
                    .collect();
                if !outside.is_empty() {
                    return Err(WcError::state(
                        "conflict_outside_sparse",
                        format!(
                            "the rebase of {} conflicts on paths outside the sparse profile; \
                             add them with `bee sparse add` and retry",
                            op.current
                                .as_ref()
                                .map(|c| c.change_id.to_text())
                                .unwrap_or_default()
                        ),
                    )
                    .with_paths(outside));
                }
                let manifest = remote.manifest(&op.base, &self.state.sparse)?.0;
                // Fetch every side and render each conflict.
                let mut ids: Vec<ObjectId> = Vec::new();
                for c in &op.conflicts {
                    for s in [&c.base, &c.ours, &c.theirs].into_iter().flatten() {
                        if s.mode != Mode::Gitlink {
                            ids.push(s.id);
                        }
                    }
                }
                let blobs = remote.blobs(&ids)?;
                let bytes_of = |e: &Option<Entry>| -> Option<SideBytes> {
                    let e = e.as_ref()?;
                    let b = blobs.get(&e.id)?.as_ref().ok()?;
                    Some(SideBytes {
                        mode: e.mode,
                        bytes: b.clone(),
                    })
                };
                for c in &mut op.conflicts {
                    let base = bytes_of(&c.base);
                    let ours = bytes_of(&c.ours);
                    let theirs = bytes_of(&c.theirs);
                    let r =
                        conflict::render(&c.kind, base.as_ref(), ours.as_ref(), theirs.as_ref());
                    c.markers = r.markers;
                    c.rendered = match r.content {
                        Some(s) => {
                            checkout::store_local(&self.layout, &s.bytes)?;
                            Some(Entry::for_bytes(s.mode, &s.bytes))
                        }
                        None => None,
                    };
                }
                let mut st = self.state.clone();
                st.head = op.base;
                st.stack = op
                    .kept
                    .iter()
                    .chain(op.done.iter())
                    .chain(op.current.iter())
                    .chain(op.pending.iter())
                    .cloned()
                    .collect();
                st.rebase = Some(op);
                let expected = state::expected_tree(&st, &manifest);
                let targets = targets_between(from, &expected);
                let mut report = self.commit_local(
                    t,
                    Commit {
                        op: op_name,
                        summary: summary.clone(),
                        state: st,
                        manifest,
                        targets,
                        soft,
                        undoable: true,
                        undoes: None,
                    },
                )?;
                report.summary = format!("{summary}: stopped on conflicts");
                let paths: Vec<String> = report.conflicts.iter().map(|c| c.path.clone()).collect();
                let json = serde_json::to_value(&report)
                    .map_err(|e| WcError::internal("encode", e.to_string()))?;
                Err(WcError::conflicts(
                    format!(
                        "{} conflicting path(s); edit them (ours = target, theirs = your change), \
                         then run `bee resolve`; or `bee sync --abort`",
                        paths.len()
                    ),
                    paths,
                )
                .with_report(json))
            }
        }
    }

    /// A rebase of stack entries `from_index..` onto `base`, keeping the ones below.
    fn restack_op(&self, from_index: usize, base: ObjectId, stack: &[StackEntry]) -> RebaseOp {
        RebaseOp {
            kind: RebaseKind::Restack,
            base,
            kept: stack[..from_index].to_vec(),
            done: Vec::new(),
            current: None,
            pending: stack[from_index..].to_vec(),
            conflicts: Vec::new(),
            clean: Vec::new(),
            dropped: Vec::new(),
            new_onto: self.state.onto,
            new_onto_version: self.state.onto_version,
            return_to: None,
            return_bottom: false,
            abort: Box::new(self.state.clone()),
        }
    }

    /// `bee commit`: records uncommitted changes as a new change after head.
    pub fn commit(&mut self, t: &dyn Transport, opts: &CommitOptions) -> Result<OpReport> {
        self.refuse_rebase("commit")?;
        let scan = self.scan()?;
        let all = self.changes(&scan);
        let chosen: Vec<Change> = all
            .iter()
            .filter(|c| under_any(&c.path, &opts.paths))
            .cloned()
            .collect();
        if chosen.is_empty() && !opts.allow_empty {
            return Err(WcError::remote(v1::ErrorDetail {
                reason: "EMPTY_CHANGE".into(),
                message: "nothing to commit: the working copy has no changes (use --allow-empty)"
                    .into(),
                subcode: "nothing_to_commit".into(),
                ..Default::default()
            }));
        }
        let insert_at = self.head_pos().map_or(0, |i| i + 1);
        let has_descendants = insert_at < self.state.stack.len();
        if has_descendants && chosen.len() != all.len() {
            return Err(WcError::dirty(
                "committing below other changes moves them; commit every change (no path \
                 filter) so the rest of the working tree is not overwritten",
                all.iter()
                    .filter(|c| !under_any(&c.path, &opts.paths))
                    .map(|c| path_text(&c.path))
                    .collect(),
            ));
        }
        let delta = self.upload_changes(t, &chosen)?;
        let mut st = self.state.clone();
        let change_id = self.next_change_id(&mut st)?;
        let cs = self.remote(t).create_changeset(&NewChangeset {
            parent: self.state.head,
            delta: &delta,
            change_id,
            author: &opts.author,
            message: &opts.message,
            provenance: opts.provenance.as_ref(),
            allow_empty: opts.allow_empty,
        })?;
        let id = from_pid(cs.id.as_ref(), "changeset id")?;
        let entry = StackEntry {
            change_id,
            changeset: id,
            parent: self.state.head,
            message: opts.message.clone(),
        };
        let mut manifest = self.manifest.clone();
        manifest::apply(&mut manifest, &delta);
        let summary = format!("commit {}: {}", change_id.to_text(), subject(&opts.message));
        let mut report = if has_descendants {
            let mut stack = self.state.stack.clone();
            stack.insert(insert_at, entry);
            let mut op = self.restack_op(insert_at + 1, id, &stack);
            op.abort.next_change = st.next_change;
            op.return_to = Some(change_id);
            let drive = self.drive(&self.remote(t), op)?;
            self.settle(
                t,
                drive,
                "commit",
                summary,
                &manifest,
                Some((id, &manifest)),
                true,
            )?
        } else {
            st.stack.insert(insert_at, entry);
            st.head = id;
            let mut r = self.commit_local(
                t,
                Commit {
                    op: "commit",
                    summary: summary.clone(),
                    state: st,
                    manifest,
                    targets: Vec::new(),
                    soft: true,
                    undoable: true,
                    undoes: None,
                },
            )?;
            r.summary = summary;
            r
        };
        report.changeset = Some(id.to_text());
        report.change_id = Some(change_id.to_text());
        Ok(report)
    }

    /// `bee amend`: folds uncommitted changes (and optionally a new message) into the head
    /// change, keeping its ChangeId; changes above it are rebased.
    pub fn amend(
        &mut self,
        t: &dyn Transport,
        message: Option<&str>,
        paths: &[String],
        author: &Signature,
    ) -> Result<OpReport> {
        self.refuse_rebase("amend")?;
        let Some(i) = self.head_pos() else {
            return Err(WcError::state(
                "nothing_to_amend",
                "head is not a local change (landed changes cannot be amended); commit first",
            ));
        };
        let scan = self.scan()?;
        let all = self.changes(&scan);
        let chosen: Vec<Change> = all
            .iter()
            .filter(|c| under_any(&c.path, paths))
            .cloned()
            .collect();
        let has_descendants = i + 1 < self.state.stack.len();
        if has_descendants && chosen.len() != all.len() {
            return Err(WcError::dirty(
                "amending below other changes moves them; amend every change (no path filter)",
                all.iter()
                    .filter(|c| !under_any(&c.path, paths))
                    .map(|c| path_text(&c.path))
                    .collect(),
            ));
        }
        if chosen.is_empty() && message.is_none() {
            return Err(WcError::remote(v1::ErrorDetail {
                reason: "EMPTY_CHANGE".into(),
                message: "nothing to amend: no changes and no new message".into(),
                subcode: "nothing_to_amend".into(),
                ..Default::default()
            }));
        }
        let entry = self.entry_at(i)?;
        let remote = self.remote(t);
        let cs = remote
            .changesets(&[entry.changeset], true)?
            .pop()
            .ok_or_else(|| WcError::internal("bad_response", "no changeset"))?;
        let d1 = delta_of(&cs)?;
        let w = self.upload_changes(t, &chosen)?;
        let both = overlap(&d1, &w);
        let parent_entries = if both.is_empty() {
            BTreeMap::new()
        } else {
            remote.entries_at(&entry.parent, &both)?
        };
        let delta = compose(&d1, &w, &parent_entries);
        let message = message.map_or(entry.message.clone(), str::to_owned);
        let sig = signature_of(cs.meta.as_ref(), author);
        let new = remote.create_changeset(&NewChangeset {
            parent: entry.parent,
            delta: &delta,
            change_id: entry.change_id,
            author: &sig,
            message: &message,
            provenance: cs.meta.as_ref().and_then(|m| m.provenance.as_ref()),
            allow_empty: true,
        })?;
        let id = from_pid(new.id.as_ref(), "changeset id")?;
        let mut manifest = self.manifest.clone();
        manifest::apply(&mut manifest, &w);
        let mut stack = self.state.stack.clone();
        stack[i] = StackEntry {
            change_id: entry.change_id,
            changeset: id,
            parent: entry.parent,
            message: message.clone(),
        };
        let summary = format!("amend {}: {}", entry.change_id.to_text(), subject(&message));
        let mut op = self.restack_op(i + 1, id, &stack);
        op.return_to = Some(entry.change_id);
        let drive = self.drive(&remote, op)?;
        let mut report = self.settle(
            t,
            drive,
            "amend",
            summary,
            &manifest,
            Some((id, &manifest)),
            true,
        )?;
        report.changeset = Some(id.to_text());
        report.change_id = Some(entry.change_id.to_text());
        Ok(report)
    }

    /// `bee describe`: replaces a change's message (default: head), keeping its content and
    /// ChangeId; changes above it are rebased.
    pub fn describe(
        &mut self,
        t: &dyn Transport,
        change: Option<&str>,
        message: &str,
        author: &Signature,
    ) -> Result<OpReport> {
        self.refuse_rebase("describe")?;
        let i = match change {
            Some(spec) => self.find_change(spec)?,
            None => self.head_pos().ok_or_else(|| {
                WcError::state("nothing_to_describe", "head is not a local change")
            })?,
        };
        if i + 1 < self.state.stack.len() {
            self.require_clean("describe")?;
        }
        let entry = self.entry_at(i)?;
        let remote = self.remote(t);
        let cs = remote
            .changesets(&[entry.changeset], true)?
            .pop()
            .ok_or_else(|| WcError::internal("bad_response", "no changeset"))?;
        let delta = delta_of(&cs)?;
        let sig = signature_of(cs.meta.as_ref(), author);
        let new = remote.create_changeset(&NewChangeset {
            parent: entry.parent,
            delta: &delta,
            change_id: entry.change_id,
            author: &sig,
            message,
            provenance: cs.meta.as_ref().and_then(|m| m.provenance.as_ref()),
            allow_empty: true,
        })?;
        let id = from_pid(new.id.as_ref(), "changeset id")?;
        let mut stack = self.state.stack.clone();
        stack[i].changeset = id;
        stack[i].message = message.to_owned();
        let mut op = self.restack_op(i + 1, id, &stack);
        let head_change = self.head_pos().map(|h| self.state.stack[h].change_id);
        op.return_to = head_change;
        let summary = format!(
            "describe {}: {}",
            entry.change_id.to_text(),
            subject(message)
        );
        // Content does not change: the working tree (dirty edits included) stays as it is.
        let from = self.expected();
        let known = (Some(entry.change_id) == head_change).then(|| self.manifest.clone());
        let drive = self.drive(&remote, op)?;
        let mut report = self.settle(
            t,
            drive,
            "describe",
            summary,
            &from,
            known.as_ref().map(|m| (id, m)),
            false,
        )?;
        report.changeset = Some(id.to_text());
        report.change_id = Some(entry.change_id.to_text());
        Ok(report)
    }

    /// `bee stack new`: an empty change after head, which becomes head. Later `amend`s
    /// fill it.
    pub fn stack_new(
        &mut self,
        t: &dyn Transport,
        message: &str,
        author: &Signature,
    ) -> Result<OpReport> {
        self.refuse_rebase("stack new")?;
        let insert_at = self.head_pos().map_or(0, |i| i + 1);
        let has_descendants = insert_at < self.state.stack.len();
        if has_descendants {
            self.require_clean("stack new")?;
        }
        let mut st = self.state.clone();
        let change_id = self.next_change_id(&mut st)?;
        let cs = self.remote(t).create_changeset(&NewChangeset {
            parent: self.state.head,
            delta: &[],
            change_id,
            author,
            message,
            provenance: None,
            allow_empty: true,
        })?;
        let id = from_pid(cs.id.as_ref(), "changeset id")?;
        let mut stack = self.state.stack.clone();
        stack.insert(
            insert_at,
            StackEntry {
                change_id,
                changeset: id,
                parent: self.state.head,
                message: message.to_owned(),
            },
        );
        let mut op = self.restack_op(insert_at + 1, id, &stack);
        op.abort.next_change = st.next_change;
        op.return_to = Some(change_id);
        let summary = format!("new {}: {}", change_id.to_text(), subject(message));
        let manifest = self.manifest.clone();
        let from = self.expected();
        let drive = self.drive(&self.remote(t), op)?;
        let mut report = self.settle(
            t,
            drive,
            "stack-new",
            summary,
            &from,
            Some((id, &manifest)),
            false,
        )?;
        report.changeset = Some(id.to_text());
        report.change_id = Some(change_id.to_text());
        Ok(report)
    }

    /// Moves head to `target` (a changeset with a known stack position or any revision),
    /// keeping uncommitted edits to paths the move does not touch.
    fn move_head(
        &mut self,
        t: &dyn Transport,
        op_name: &'static str,
        summary: String,
        mut st: State,
    ) -> Result<OpReport> {
        let scan = self.scan()?;
        let dirty: BTreeSet<Vec<u8>> = self.dirty(&scan).into_iter().collect();
        let (manifest, _) = self.remote(t).manifest(&st.head, &st.sparse)?;
        st.rebase = None;
        let from = self.expected();
        let to = state::expected_tree(&st, &manifest);
        let targets = targets_between(&from, &to);
        let clash: Vec<String> = targets
            .iter()
            .filter(|(p, _)| dirty.contains(&p.0))
            .map(|(p, _)| path_text(&p.0))
            .collect();
        // An untracked file where the target puts a file, or under a path that becomes a file.
        let mut blocked: Vec<String> = Vec::new();
        for (p, e) in &targets {
            if e.is_some()
                && !from.contains_key(&p.0)
                && scan.actual.contains_key(&p.0)
                && !dirty.contains(&p.0)
            {
                blocked.push(path_text(&p.0));
            }
        }
        if !clash.is_empty() || !blocked.is_empty() {
            let mut all = clash;
            all.extend(blocked);
            all.sort();
            all.dedup();
            return Err(WcError::dirty(
                format!(
                    "{op_name} refused: it would overwrite uncommitted changes in {} path(s); \
                     commit them first",
                    all.len()
                ),
                all,
            ));
        }
        let changeset = st.head.to_text();
        let mut report = self.commit_local(
            t,
            Commit {
                op: op_name,
                summary: summary.clone(),
                state: st,
                manifest,
                targets,
                soft: false,
                undoable: true,
                undoes: None,
            },
        )?;
        report.summary = summary;
        report.changeset = Some(changeset);
        Ok(report)
    }

    /// `bee stack edit`: checks out a change of the stack, so `amend` rewrites it and
    /// `commit` inserts after it. Uncommitted edits to untouched paths are kept.
    pub fn stack_edit(&mut self, t: &dyn Transport, change: &str) -> Result<OpReport> {
        self.refuse_rebase("stack edit")?;
        let i = self.find_change(change)?;
        let entry = self.entry_at(i)?;
        let mut st = self.state.clone();
        st.head = entry.changeset;
        let mut report = self.move_head(
            t,
            "stack-edit",
            format!(
                "edit {}: {}",
                entry.change_id.to_text(),
                subject(&entry.message)
            ),
            st,
        )?;
        report.change_id = Some(entry.change_id.to_text());
        Ok(report)
    }

    /// `bee goto`: checks out a stack change, the stack bottom (`@base`), or any revision.
    /// Leaving a non-empty stack for another revision needs `discard_stack` (undo restores
    /// it). Uncommitted edits to untouched paths are kept.
    pub fn goto(&mut self, t: &dyn Transport, rev: &str, discard_stack: bool) -> Result<OpReport> {
        self.refuse_rebase("goto")?;
        if rev == "@base" {
            let mut st = self.state.clone();
            st.head = st.onto;
            return self.move_head(t, "goto", "goto the stack base".into(), st);
        }
        if let Ok(i) = self.find_change(rev) {
            return self.stack_edit(t, &(i + 1).to_string());
        }
        let id = self.remote(t).resolve(rev)?;
        if let Some(i) = self.state.stack.iter().position(|e| e.changeset == id) {
            return self.stack_edit(t, &(i + 1).to_string());
        }
        if id == self.state.onto {
            let mut st = self.state.clone();
            st.head = id;
            return self.move_head(t, "goto", format!("goto {rev}"), st);
        }
        if !self.state.stack.is_empty() && !discard_stack {
            return Err(WcError::state(
                "stack_not_empty",
                format!(
                    "the stack has {} unlanded change(s); land them, or pass --discard-stack \
                     (`bee undo` brings them back)",
                    self.state.stack.len()
                ),
            ));
        }
        let mut st = self.state.clone();
        st.head = id;
        st.onto = id;
        st.onto_version = 0;
        st.stack = Vec::new();
        self.move_head(t, "goto", format!("goto {rev} ({})", id.to_text()), st)
    }

    /// `bee stack squash`: folds a change (default head) into its parent change, which
    /// keeps its ChangeId and message (or takes `message`).
    pub fn squash(
        &mut self,
        t: &dyn Transport,
        change: Option<&str>,
        message: Option<&str>,
        author: &Signature,
    ) -> Result<OpReport> {
        self.refuse_rebase("stack squash")?;
        let i = match change {
            Some(spec) => self.find_change(spec)?,
            None => self
                .head_pos()
                .ok_or_else(|| WcError::state("nothing_to_squash", "head is not a local change"))?,
        };
        if i == 0 {
            return Err(WcError::state(
                "no_parent_in_stack",
                "the bottom change has no parent change to squash into",
            ));
        }
        self.require_clean("stack squash")?;
        let child = self.entry_at(i)?;
        let parent = self.entry_at(i - 1)?;
        let remote = self.remote(t);
        let mut both = remote.changesets(&[parent.changeset, child.changeset], true)?;
        let ccs = both
            .pop()
            .ok_or_else(|| WcError::internal("bad_response", "no changeset"))?;
        let pcs = both
            .pop()
            .ok_or_else(|| WcError::internal("bad_response", "no changeset"))?;
        let d1 = delta_of(&pcs)?;
        let d2 = delta_of(&ccs)?;
        let touched = overlap(&d1, &d2);
        let gp_entries = if touched.is_empty() {
            BTreeMap::new()
        } else {
            remote.entries_at(&parent.parent, &touched)?
        };
        let delta = compose(&d1, &d2, &gp_entries);
        let message = message.map_or(parent.message.clone(), str::to_owned);
        let sig = signature_of(pcs.meta.as_ref(), author);
        let new = remote.create_changeset(&NewChangeset {
            parent: parent.parent,
            delta: &delta,
            change_id: parent.change_id,
            author: &sig,
            message: &message,
            provenance: pcs.meta.as_ref().and_then(|m| m.provenance.as_ref()),
            allow_empty: true,
        })?;
        let id = from_pid(new.id.as_ref(), "changeset id")?;
        let mut stack = self.state.stack.clone();
        stack[i - 1] = StackEntry {
            change_id: parent.change_id,
            changeset: id,
            parent: parent.parent,
            message: message.clone(),
        };
        stack.remove(i);
        let head = self.head_pos();
        let mut op = self.restack_op(i, id, &stack);
        op.return_to = match head {
            Some(h) if h == i || h == i - 1 => Some(parent.change_id),
            Some(h) => Some(self.state.stack[h].change_id),
            None => None,
        };
        let summary = format!(
            "squash {} into {}",
            child.change_id.to_text(),
            parent.change_id.to_text()
        );
        let from = self.expected();
        let known_manifest = if head == Some(i) {
            Some(self.manifest.clone())
        } else {
            None
        };
        let drive = self.drive(&remote, op)?;
        let mut report = self.settle(
            t,
            drive,
            "stack-squash",
            summary,
            &from,
            known_manifest.as_ref().map(|m| (id, m)),
            false,
        )?;
        report.changeset = Some(id.to_text());
        report.change_id = Some(parent.change_id.to_text());
        Ok(report)
    }

    /// `bee stack split`: moves the paths under `paths` of a change (default head) into a
    /// new change inserted before it; the original keeps the rest, its ChangeId and message.
    pub fn split(
        &mut self,
        t: &dyn Transport,
        change: Option<&str>,
        paths: &[String],
        message: Option<&str>,
        author: &Signature,
    ) -> Result<OpReport> {
        self.refuse_rebase("stack split")?;
        if paths.is_empty() {
            return Err(WcError::usage(
                "missing_paths",
                "stack split needs at least one path",
            ));
        }
        let i = match change {
            Some(spec) => self.find_change(spec)?,
            None => self
                .head_pos()
                .ok_or_else(|| WcError::state("nothing_to_split", "head is not a local change"))?,
        };
        self.require_clean("stack split")?;
        let entry = self.entry_at(i)?;
        let remote = self.remote(t);
        let cs = remote
            .changesets(&[entry.changeset], true)?
            .pop()
            .ok_or_else(|| WcError::internal("bad_response", "no changeset"))?;
        let delta = delta_of(&cs)?;
        let (first, rest): (Vec<_>, Vec<_>) =
            delta.into_iter().partition(|(p, _)| under_any(p, paths));
        if first.is_empty() || rest.is_empty() {
            return Err(WcError::usage(
                "split_would_be_empty",
                format!(
                    "the paths select {} of the change's {} path(s); both parts must be non-empty",
                    first.len(),
                    first.len() + rest.len()
                ),
            ));
        }
        let sig = signature_of(cs.meta.as_ref(), author);
        let mut st = self.state.clone();
        let first_id = self.next_change_id(&mut st)?;
        let first_message = message.map_or_else(
            || format!("{} (split)", subject(&entry.message)),
            str::to_owned,
        );
        let a = remote.create_changeset(&NewChangeset {
            parent: entry.parent,
            delta: &first,
            change_id: first_id,
            author: &sig,
            message: &first_message,
            provenance: cs.meta.as_ref().and_then(|m| m.provenance.as_ref()),
            allow_empty: false,
        })?;
        let a_id = from_pid(a.id.as_ref(), "changeset id")?;
        let b = remote.create_changeset(&NewChangeset {
            parent: a_id,
            delta: &rest,
            change_id: entry.change_id,
            author: &sig,
            message: &entry.message,
            provenance: cs.meta.as_ref().and_then(|m| m.provenance.as_ref()),
            allow_empty: false,
        })?;
        let b_id = from_pid(b.id.as_ref(), "changeset id")?;
        let mut stack = self.state.stack.clone();
        stack[i] = StackEntry {
            change_id: entry.change_id,
            changeset: b_id,
            parent: a_id,
            message: entry.message.clone(),
        };
        stack.insert(
            i,
            StackEntry {
                change_id: first_id,
                changeset: a_id,
                parent: entry.parent,
                message: first_message,
            },
        );
        let mut op = self.restack_op(i + 2, b_id, &stack);
        op.abort.next_change = st.next_change;
        op.return_to = self.head_pos().map(|h| self.state.stack[h].change_id);
        let summary = format!(
            "split {} paths out of {} into {}",
            first.len(),
            entry.change_id.to_text(),
            first_id.to_text()
        );
        let from = self.expected();
        let known = if self.head_pos() == Some(i) {
            Some(self.manifest.clone())
        } else {
            None
        };
        let drive = self.drive(&remote, op)?;
        let mut report = self.settle(
            t,
            drive,
            "stack-split",
            summary,
            &from,
            known.as_ref().map(|m| (b_id, m)),
            false,
        )?;
        report.changeset = Some(a_id.to_text());
        report.change_id = Some(first_id.to_text());
        Ok(report)
    }

    /// `bee sync` (alias `rebase`): rebases the stack onto the target ref's tip
    /// server-side and checks out the result. Conflicts are written with diff3 markers and
    /// stop the rebase (exit 3) until `bee resolve` or `bee sync --abort`.
    pub fn sync(&mut self, t: &dyn Transport) -> Result<OpReport> {
        self.refuse_rebase("sync")?;
        let scan = self.require_clean("sync")?;
        let remote = self.remote(t);
        let (tip, version) =
            remote.resolve_ref(&self.config.target_ref, self.state.onto_version)?;
        let bottom_parent = self
            .state
            .stack
            .first()
            .map_or(self.state.head, |e| e.parent);
        if tip == self.state.onto
            && bottom_parent == tip
            && (self.state.head == tip || self.head_pos().is_some())
        {
            let mut r = self.noop_report("sync", "already up to date");
            r.changeset = Some(tip.to_text());
            return Ok(r);
        }
        let op = RebaseOp {
            kind: RebaseKind::Sync,
            base: tip,
            kept: Vec::new(),
            done: Vec::new(),
            current: None,
            pending: self.state.stack.clone(),
            conflicts: Vec::new(),
            clean: Vec::new(),
            dropped: Vec::new(),
            new_onto: tip,
            new_onto_version: version,
            return_to: self.head_pos().map(|h| self.state.stack[h].change_id),
            return_bottom: false,
            abort: Box::new(self.state.clone()),
        };
        let summary = format!(
            "sync {} change(s) onto {}@{version}",
            self.state.stack.len(),
            self.config.target_ref
        );
        let drive = self.drive(&remote, op)?;
        let mut report = self.settle(t, drive, "sync", summary, &scan.actual, None, false)?;
        report.changeset = Some(tip.to_text());
        Ok(report)
    }

    /// `bee sync --abort`: abandons a rebase in progress and restores the state before it.
    pub fn abort(&mut self, t: &dyn Transport) -> Result<OpReport> {
        let Some(op) = self.state.rebase.clone() else {
            return Err(WcError::state("no_rebase", "no rebase is in progress"));
        };
        let scan = self.scan()?;
        let mut st = (*op.abort).clone();
        st.next_change = st.next_change.max(self.state.next_change);
        let (manifest, _) = self.remote(t).manifest(&st.head, &st.sparse)?;
        let to = state::expected_tree(&st, &manifest);
        let targets = targets_between(&scan.actual, &to);
        let summary = "abort the rebase".to_owned();
        let mut report = self.commit_local(
            t,
            Commit {
                op: "abort",
                summary: summary.clone(),
                state: st,
                manifest,
                targets,
                soft: false,
                undoable: true,
                undoes: None,
            },
        )?;
        report.summary = summary;
        Ok(report)
    }

    /// `bee resolve`: marks conflicts resolved (checking that their markers are gone, or
    /// taking a side with `pick`); once every conflict is resolved, records the resolution
    /// as the change (same ChangeId and message) and continues the rebase.
    pub fn resolve(
        &mut self,
        t: &dyn Transport,
        paths: &[String],
        pick: Option<Pick>,
        author: &Signature,
    ) -> Result<OpReport> {
        let Some(mut op) = self.state.rebase.clone() else {
            return Err(WcError::state(
                "no_rebase",
                "no rebase is in progress; nothing to resolve",
            ));
        };
        if op.current.is_none() {
            return Err(WcError::state(
                "no_conflicts",
                "the rebase has no conflicts to resolve",
            ));
        }
        let selected: Vec<usize> = op
            .conflicts
            .iter()
            .enumerate()
            .filter(|(_, c)| under_any(&c.path.0, paths))
            .map(|(i, _)| i)
            .collect();
        if selected.is_empty() {
            return Err(WcError::usage(
                "no_such_conflict",
                "no conflicting path matches (see `bee status`)",
            ));
        }
        if let Some(pick) = pick {
            let remote = self.remote(t);
            let ids: Vec<ObjectId> = selected
                .iter()
                .filter_map(|&i| {
                    let c = &op.conflicts[i];
                    match pick {
                        Pick::Ours => c.ours,
                        Pick::Theirs => c.theirs,
                        Pick::Base => c.base,
                    }
                    .map(|e| e.id)
                })
                .collect();
            let blobs = remote.blobs(&ids)?;
            for &i in &selected {
                let c = &op.conflicts[i];
                let side = match pick {
                    Pick::Ours => c.ours,
                    Pick::Theirs => c.theirs,
                    Pick::Base => c.base,
                };
                let path = fs_path(&self.root, &c.path.0)?;
                match side {
                    Some(e) => {
                        let bytes = match blobs.get(&e.id) {
                            Some(Ok(b)) => b.clone(),
                            _ => {
                                return Err(WcError::state(
                                    "side_not_readable",
                                    format!("{}: that side cannot be read", path_text(&c.path.0)),
                                ));
                            }
                        };
                        checkout::store_local(&self.layout, &bytes)?;
                        let mut fetch = |_: &[ObjectId]| Ok(BTreeMap::new());
                        checkout::apply(
                            &self.root,
                            &self.layout,
                            &[(c.path.clone(), Some(e))],
                            &BTreeSet::new(),
                            &mut fetch,
                            &journal::Faults::none(),
                            &self.worktree_attributes(&[])?,
                        )?;
                    }
                    None => {
                        fsutil::remove_optional(&path)?;
                    }
                }
            }
        }
        let scan = self.scan()?;
        let mut still: Vec<String> = Vec::new();
        for &i in &selected {
            let c = &mut op.conflicts[i];
            if c.markers && pick.is_none() {
                let path = fs_path(&self.root, &c.path.0)?;
                if let Some(bytes) = fsutil::read_optional(&path)?
                    && conflict::has_markers(&bytes)
                {
                    still.push(path_text(&c.path.0));
                    continue;
                }
            }
            c.resolved = true;
        }
        if !still.is_empty() {
            return Err(WcError::conflicts(
                format!(
                    "{} file(s) still hold conflict markers; edit them, or pick a side with \
                     --ours/--theirs",
                    still.len()
                ),
                still,
            ));
        }
        let unresolved = op.conflicts.iter().filter(|c| !c.resolved).count();
        if unresolved > 0 {
            // Record progress only.
            let mut st = self.state.clone();
            st.rebase = Some(op);
            let summary = format!("resolve: {unresolved} conflict(s) left");
            let manifest = self.manifest.clone();
            let mut report = self.commit_local(
                t,
                Commit {
                    op: "resolve",
                    summary: summary.clone(),
                    state: st,
                    manifest,
                    targets: Vec::new(),
                    soft: true,
                    undoable: true,
                    undoes: None,
                },
            )?;
            report.summary = summary;
            return Ok(report);
        }
        // Every conflict is resolved: record the change and continue.
        let current = op
            .current
            .clone()
            .ok_or_else(|| WcError::internal("state", "no current change"))?;
        let remote = self.remote(t);
        let original = remote
            .changesets(&[current.changeset], true)?
            .pop()
            .ok_or_else(|| WcError::internal("bad_response", "no changeset"))?;
        let changes = manifest::diff(&self.manifest, &scan.actual);
        let delta = self.upload_changes(t, &changes)?;
        if delta.is_empty() {
            op.dropped.push(current.change_id.to_text());
        } else {
            let sig = signature_of(original.meta.as_ref(), author);
            let cs = remote.create_changeset(&NewChangeset {
                parent: op.base,
                delta: &delta,
                change_id: current.change_id,
                author: &sig,
                message: &current.message,
                provenance: original.meta.as_ref().and_then(|m| m.provenance.as_ref()),
                allow_empty: false,
            })?;
            let id = from_pid(cs.id.as_ref(), "changeset id")?;
            op.done.push(StackEntry {
                change_id: current.change_id,
                changeset: id,
                parent: op.base,
                message: current.message.clone(),
            });
            op.base = id;
        }
        op.current = None;
        op.conflicts = Vec::new();
        op.clean = Vec::new();
        let kind = op.kind;
        let drive = self.drive(&remote, op)?;
        let name = match kind {
            RebaseKind::Sync => "sync",
            RebaseKind::Restack => "restack",
        };
        let summary = format!(
            "resolve {} and continue the {name}",
            current.change_id.to_text()
        );
        let mut report = self.settle(t, drive, "resolve", summary, &scan.actual, None, false)?;
        report.change_id = Some(current.change_id.to_text());
        Ok(report)
    }

    /// `bee land`: lands the stack up to head on the target ref (Land, through the
    /// server's coalescer). On a conflict with the target, syncs so the conflicts are
    /// materialized (exit 3). Changes above head are rebased onto the landed tip.
    pub fn land(&mut self, t: &dyn Transport) -> Result<OpReport> {
        self.refuse_rebase("land")?;
        let Some(top) = self.head_pos() else {
            return Err(WcError::state(
                "nothing_to_land",
                "nothing to land: the stack is empty (commit first) or head is at its base",
            ));
        };
        let scan = self.require_clean("land")?;
        let head = self.state.head;
        let remote = self.remote(t);
        let resp = match remote.land(&self.config.target_ref, &head) {
            Ok(r) => r,
            Err(e) if e.is_remote_reason("MERGE_CONFLICT") => {
                // Bring the conflicts into the working tree.
                return match self.sync(t) {
                    Ok(_) => Err(WcError::state(
                        "synced",
                        "the stack conflicted with the target and was synced cleanly; run \
                         `bee land` again",
                    )),
                    Err(err) => Err(err),
                };
            }
            Err(e) => return Err(e),
        };
        let r = resp
            .r#ref
            .clone()
            .ok_or_else(|| WcError::internal("bad_response", "Land returned no ref"))?;
        let tip = from_pid(r.target.as_ref(), "ref target")?;
        let mut landed = Vec::new();
        for l in &resp.landed {
            landed.push(LandedView {
                change_id: crate::remote::from_pchange(l.change_id.as_ref())?.to_text(),
                source: from_pid(l.source.as_ref(), "source")?.to_text(),
                result: from_pid(l.result.as_ref(), "result")?.to_text(),
                skipped: l.skipped,
            });
        }
        let land = LandView {
            r#ref: r.name.clone(),
            target: tip.to_text(),
            version: r.version,
            attempts: resp.attempts,
            event_seq: resp.event_seq,
            landed,
        };
        let rest: Vec<StackEntry> = self.state.stack[top + 1..].to_vec();
        let mut st = self.state.clone();
        st.stack = Vec::new();
        st.head = tip;
        st.onto = tip;
        st.onto_version = r.version;
        let summary = format!(
            "land {} change(s) on {}@{}",
            top + 1,
            self.config.target_ref,
            r.version
        );
        let mut report = if rest.is_empty() {
            let (manifest, _) = remote.manifest(&tip, &st.sparse)?;
            let targets = targets_between(&scan.actual, &manifest);
            self.commit_local(
                t,
                Commit {
                    op: "land",
                    summary: summary.clone(),
                    state: st,
                    manifest,
                    targets,
                    soft: false,
                    undoable: false,
                    undoes: None,
                },
            )?
        } else {
            let op = RebaseOp {
                kind: RebaseKind::Restack,
                base: tip,
                kept: Vec::new(),
                done: Vec::new(),
                current: None,
                pending: rest,
                conflicts: Vec::new(),
                clean: Vec::new(),
                dropped: Vec::new(),
                new_onto: tip,
                new_onto_version: r.version,
                return_to: None,
                return_bottom: true,
                abort: Box::new(st),
            };
            let drive = self.drive(&remote, op)?;
            self.settle(t, drive, "land", summary.clone(), &scan.actual, None, false)?
        };
        report.summary = summary;
        report.changeset = Some(tip.to_text());
        report.land = Some(land);
        Ok(report)
    }

    /// `bee sparse set/add/remove`: changes the sparse profile and materializes or removes
    /// files accordingly. Paths leaving the profile must not have uncommitted changes.
    pub fn set_sparse(&mut self, t: &dyn Transport, sparse: Sparse) -> Result<OpReport> {
        self.refuse_rebase("sparse")?;
        if sparse == self.state.sparse {
            return Ok(self.noop_report("sparse", "the profile is unchanged"));
        }
        let scan = self.scan()?;
        let dirty: Vec<String> = self
            .dirty(&scan)
            .into_iter()
            .filter(|p| !sparse.contains(p))
            .map(|p| path_text(&p))
            .collect();
        if !dirty.is_empty() {
            return Err(WcError::dirty(
                "paths leaving the sparse profile have uncommitted changes; commit them first",
                dirty,
            ));
        }
        let mut st = self.state.clone();
        st.sparse = sparse.clone();
        let (manifest, _) = self.remote(t).manifest(&st.head, &sparse)?;
        let targets = targets_between(&self.manifest, &manifest);
        let summary = format!("sparse {}", sparse.display().join(" "));
        let mut report = self.commit_local(
            t,
            Commit {
                op: "sparse",
                summary: summary.clone(),
                state: st,
                manifest,
                targets,
                soft: false,
                undoable: true,
                undoes: None,
            },
        )?;
        report.summary = summary;
        Ok(report)
    }

    /// `bee undo`: reverts the latest local operation that was not undone (commit, amend,
    /// sync, stack edits, sparse changes, resolve). Server objects stay; a `land` cannot be
    /// undone locally.
    pub fn undo(&mut self, t: &dyn Transport) -> Result<OpReport> {
        let log = self.oplog()?;
        let undone: BTreeSet<u64> = log.iter().filter_map(|r| r.undoes).collect();
        let target: Option<&OpRecord> = log
            .iter()
            .rev()
            .find(|r| r.undoes.is_none() && !undone.contains(&r.seq));
        let Some(record) = target else {
            return Err(WcError::state(
                "nothing_to_undo",
                "there is no operation to undo",
            ));
        };
        if !record.undoable {
            return Err(WcError::state(
                "not_undoable",
                format!(
                    "the last operation ({}) changed the server or created the working copy and \
                     cannot be undone locally{}",
                    record.op,
                    if record.op == "land" {
                        "; revert it with `bee write revert`"
                    } else {
                        ""
                    }
                ),
            ));
        }
        let mut before_manifest = self.manifest.clone();
        for (p, e) in &record.manifest_undo {
            match e {
                Some(e) => {
                    before_manifest.insert(p.0.clone(), *e);
                }
                None => {
                    before_manifest.remove(&p.0);
                }
            }
        }
        let mut st = record.before.clone();
        st.next_change = st.next_change.max(self.state.next_change);
        let targets = if record.soft {
            Vec::new()
        } else {
            let scan = self.require_clean("undo")?;
            let to = state::expected_tree(&st, &before_manifest);
            targets_between(&scan.actual, &to)
        };
        let summary = format!("undo {} ({})", record.op, record.summary);
        let seq = record.seq;
        let op = record.op.clone();
        let mut report = self.commit_local(
            t,
            Commit {
                op: "undo",
                summary: summary.clone(),
                state: st,
                manifest: before_manifest,
                targets,
                soft: record.soft,
                undoable: false,
                undoes: Some(seq),
            },
        )?;
        report.summary = summary;
        report.undid = Some(op);
        Ok(report)
    }

    /// `bee diff` in a working copy: uncommitted changes as unified diffs. Base contents
    /// come from the server (they are not kept locally).
    pub fn diff(
        &mut self,
        t: &dyn Transport,
        paths: &[String],
        context: usize,
    ) -> Result<Vec<FileDiff>> {
        let scan = self.scan()?;
        let changes: Vec<Change> = self
            .changes(&scan)
            .into_iter()
            .filter(|c| under_any(&c.path, paths))
            .collect();
        let ids: Vec<ObjectId> = changes.iter().filter_map(|c| c.old.map(|e| e.id)).collect();
        let mut old_bytes: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
        let mut need_remote = Vec::new();
        for id in ids {
            match fsutil::read_optional(&self.layout.object(&id))? {
                Some(b) => {
                    old_bytes.insert(id, b);
                }
                None => need_remote.push(id),
            }
        }
        if !need_remote.is_empty() {
            for (id, r) in self.remote(t).blobs(&need_remote)? {
                if let Ok(b) = r {
                    old_bytes.insert(id, b);
                }
            }
        }
        let extra: Vec<Vec<u8>> = changes.iter().map(|c| c.path.clone()).collect();
        let attrs = self.worktree_attributes(&extra)?;
        let mut out = Vec::new();
        for c in changes {
            let old = c
                .old
                .and_then(|e| old_bytes.get(&e.id).cloned())
                .unwrap_or_default();
            let new = match c.new {
                Some(_) => {
                    let path = fs_path(&self.root, &c.path)?;
                    let meta = fs::symlink_metadata(&path)
                        .map_err(|e| WcError::io(format!("stat {}", path.display()), &e))?;
                    read_entry_with(&path, &meta, &c.path, &attrs)?.1
                }
                None => Vec::new(),
            };
            let binary = bee_merge::is_binary(&old, bee_merge::DEFAULT_MAX_TEXT_BYTES)
                || bee_merge::is_binary(&new, bee_merge::DEFAULT_MAX_TEXT_BYTES);
            let (unified, additions, deletions) = if binary {
                (String::new(), 0, 0)
            } else {
                let stats = bee_merge::DiffStats::between(&old, &new);
                let body = bee_merge::unified_diff(&old, &new, context);
                let name = path_text(&c.path);
                let a = if c.old.is_some() {
                    format!("a/{name}")
                } else {
                    "/dev/null".into()
                };
                let b = if c.new.is_some() {
                    format!("b/{name}")
                } else {
                    "/dev/null".into()
                };
                let text = if body.is_empty() {
                    String::new()
                } else {
                    format!("--- {a}\n+++ {b}\n{}", String::from_utf8_lossy(&body))
                };
                (text, stats.added, stats.removed)
            };
            out.push(FileDiff {
                path: path_text(&c.path),
                status: match (&c.old, &c.new) {
                    (None, _) => "added",
                    (_, None) => "deleted",
                    _ => "modified",
                }
                .into(),
                old_mode: c.old.map(|e| mode_name(e.mode)),
                new_mode: c.new.map(|e| mode_name(e.mode)),
                binary,
                additions,
                deletions,
                unified,
            });
        }
        Ok(out)
    }
}

fn side(s: Option<&v1::ConflictSide>) -> Result<Option<Entry>> {
    let Some(s) = s else { return Ok(None) };
    let Some(blob) = s.blob.as_ref().filter(|b| !b.raw.is_empty()) else {
        return Ok(None);
    };
    let id = from_pid(Some(blob), "conflict side")?;
    let mode = from_pmode(s.mode).unwrap_or(Mode::File);
    Ok(Some(Entry { mode, id, size: 0 }))
}

/// The ChangeId of a stack entry, for callers that print it.
pub fn change_text(id: &ChangeId) -> String {
    id.to_text()
}

/// The config of `root`, for callers that need it without opening.
pub fn config_of(root: &Path) -> Result<Config> {
    crate::wc::read_config(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(t: &str) -> Option<Entry> {
        Some(Entry::for_bytes(Mode::File, t.as_bytes()))
    }

    #[test]
    fn compose_drops_paths_that_return_to_the_parent() {
        let d1 = vec![
            (b"a".to_vec(), e("a1")),
            (b"b".to_vec(), e("b1")),
            (b"new".to_vec(), e("n")),
        ];
        let d2 = vec![
            (b"a".to_vec(), e("a0")),
            (b"c".to_vec(), None),
            (b"new".to_vec(), None),
        ];
        let parent: BTreeMap<Vec<u8>, Option<Entry>> =
            [(b"a".to_vec(), e("a0")), (b"new".to_vec(), None)].into();
        let out = compose(&d1, &d2, &parent);
        let paths: Vec<&[u8]> = out.iter().map(|(p, _)| p.as_slice()).collect();
        assert_eq!(paths, vec![&b"b"[..], b"c"]);
        assert_eq!(overlap(&d1, &d2), vec![b"a".to_vec(), b"new".to_vec()]);
    }

    #[test]
    fn pathspecs() {
        assert!(under_any(b"src/a.rs", &["src".into()]));
        assert!(under_any(b"src/a.rs", &["src/".into()]));
        assert!(!under_any(b"srcx/a.rs", &["src".into()]));
        assert!(under_any(b"x", &[]));
        assert!(under_any(b"x", &[".".into()]));
    }
}
