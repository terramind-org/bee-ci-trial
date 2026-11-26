//! The working copy: open (with crash recovery), status, and the local commit protocol that
//! every mutating operation goes through. The operations themselves live in [`crate::ops`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bee_core::{ChangeId, Mode, ObjectId};

use crate::checkout::{self, Applied};
use crate::error::{Result, WcError};
use crate::fsutil;
use crate::journal::{self, Faults, Layout, Lock, OpRecord, Pending};
use crate::manifest::{self, Change, Entry, Manifest};
use crate::remote::{Remote, Transport};
use crate::report::{self, OpReport, StatusReport};
use crate::scan::{self, Scan, StatCache};
use crate::state::{self, Config, HiddenPath, PathBytes, State};

/// Name of the state directory.
pub const BEE_DIR: &str = ".bee";

/// How a working copy is opened.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// Armed fault point, for crash tests.
    pub faults: Faults,
    /// How long to wait for another command's lock.
    pub lock_wait: Duration,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            faults: Faults::none(),
            lock_wait: Duration::from_secs(60),
        }
    }
}

/// An open, locked working copy.
pub struct WorkingCopy {
    pub(crate) root: PathBuf,
    pub(crate) layout: Layout,
    pub(crate) config: Config,
    pub(crate) state: State,
    pub(crate) manifest: Manifest,
    pub(crate) cache: StatCache,
    pub(crate) faults: Faults,
    pub(crate) recovered: Option<String>,
    /// A new working copy's config, written once its first intent is durable: until then
    /// the directory is not a working copy, so a crash leaves nothing half-made.
    pending_config: Option<Config>,
    _lock: Lock,
}

/// The new local state an operation commits.
pub(crate) struct Commit {
    pub op: &'static str,
    pub summary: String,
    pub state: State,
    pub manifest: Manifest,
    /// Working-tree paths to change (`None` = remove).
    pub targets: Vec<(PathBytes, Option<Entry>)>,
    pub soft: bool,
    pub undoable: bool,
    pub undoes: Option<u64>,
}

/// Finds the working copy containing `start`: the nearest ancestor with `.bee/config.json`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(BEE_DIR).join("config.json").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Reads a working copy's config without locking it (to learn its endpoint and repo).
pub fn read_config(root: &Path) -> Result<Config> {
    let layout = Layout::new(root.join(BEE_DIR));
    let bytes = fsutil::read_optional(&layout.config())?.ok_or_else(|| {
        WcError::state(
            "not_a_working_copy",
            format!("{} is not a bee working copy", root.display()),
        )
    })?;
    state::parse("config.json", &bytes)
}

fn load_cache(layout: &Layout) -> StatCache {
    // The stat cache is only a cache: a missing or damaged one means "hash everything".
    fsutil::read_optional(&layout.statcache())
        .ok()
        .flatten()
        .and_then(|b| StatCache::decode(&b).ok())
        .unwrap_or_default()
}

impl WorkingCopy {
    /// Opens and locks the working copy at `root`, finishing any operation a crash
    /// interrupted (which may fetch blobs through `transport`).
    pub fn open(root: &Path, transport: &dyn Transport, opts: &OpenOptions) -> Result<Self> {
        let layout = Layout::new(root.join(BEE_DIR));
        let config = read_config(root)?;
        let lock = Lock::acquire(&layout, opts.lock_wait)?;
        let state: State = state::read_state(&layout.dir)?.ok_or_else(|| {
            WcError::integrity("missing_state", "the working copy has no state.toml")
        })?;
        if !layout.state().exists() {
            // Written by an older client: replace it with the canonical TOML form.
            fsutil::atomic_write(&layout.state(), &state::to_toml(&state)?)?;
            let _ = fs::remove_file(layout.legacy_state());
        }
        let manifest = match fsutil::read_optional(&layout.manifest())? {
            Some(b) => manifest::decode(&b)?,
            None => Manifest::new(),
        };
        let mut wc = WorkingCopy {
            root: root.to_path_buf(),
            cache: load_cache(&layout),
            layout,
            config,
            state,
            manifest,
            faults: opts.faults.clone(),
            recovered: None,
            pending_config: None,
            _lock: lock,
        };
        wc.recover(transport)?;
        write_conflicts(&wc.layout, &wc.state)?;
        Ok(wc)
    }

    /// Creates `.bee/` in `root` (which must exist) and locks it.
    pub(crate) fn create_locked(
        root: &Path,
        config: Config,
        state: State,
        opts: &OpenOptions,
    ) -> Result<Self> {
        let layout = Layout::new(root.join(BEE_DIR));
        fs::create_dir_all(layout.journal())
            .map_err(|e| WcError::io(format!("create {}", layout.dir.display()), &e))?;
        let lock = Lock::acquire(&layout, opts.lock_wait)?;
        fsutil::atomic_write(&layout.state(), &state::to_toml(&state)?)?;
        fsutil::atomic_write(&layout.manifest(), &manifest::encode(&Manifest::new()))?;
        Ok(WorkingCopy {
            root: root.to_path_buf(),
            cache: StatCache::default(),
            layout,
            config: config.clone(),
            state,
            manifest: Manifest::new(),
            faults: opts.faults.clone(),
            recovered: None,
            pending_config: Some(config),
            _lock: lock,
        })
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The current state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The base manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The operation crash recovery finished when opening, if any.
    pub fn recovered(&self) -> Option<&str> {
        self.recovered.as_deref()
    }

    /// The operation log, oldest first.
    pub fn oplog(&self) -> Result<Vec<OpRecord>> {
        journal::read_oplog(&self.layout)
    }

    pub(crate) fn remote<'t>(&self, t: &'t dyn Transport) -> Remote<'t> {
        Remote::new(t, &self.config.repo)
    }

    fn recover(&mut self, t: &dyn Transport) -> Result<()> {
        match journal::read_pending(&self.layout)? {
            Some(pending) => {
                let op = pending.record.op.clone();
                self.finish(t, pending)?;
                self.recovered = Some(op);
            }
            None => {
                // An intent that never became durable: drop its leftovers.
                if self.layout.manifest_next().exists() {
                    journal::clear_pending(&self.layout)?;
                }
            }
        }
        Ok(())
    }

    /// The next ChangeId: BLAKE3 of the working copy's seed and a counter, so ids are unique
    /// across working copies and reproducible from the state.
    pub(crate) fn next_change_id(&self, state: &mut State) -> Result<ChangeId> {
        let seed = bee_core::hex::decode(&self.config.seed)
            .ok_or_else(|| WcError::integrity("corrupt_file", "config.json: bad seed"))?;
        let mut input = b"swarm-wc-change\0".to_vec();
        input.extend_from_slice(&seed);
        input.extend_from_slice(&state.next_change.to_be_bytes());
        state.next_change += 1;
        let d = bee_object::digest(&input);
        let mut out = [0u8; 16];
        out.copy_from_slice(&d[..16]);
        Ok(ChangeId::new(out))
    }

    /// Walks the working tree (no network), refreshing the stat cache.
    pub(crate) fn scan(&mut self) -> Result<Scan> {
        let clock = scan::touch_clock(&self.layout.dir)?;
        let result = scan::scan(
            &self.root,
            clock,
            &self.state.sparse,
            &self.manifest,
            &self.state.hidden_set(),
            &self.cache,
        )?;
        if result.cache_changed {
            fsutil::atomic_write(&self.layout.statcache(), &result.cache.encode())?;
        }
        self.cache = result.cache.clone();
        Ok(result)
    }

    /// Uncommitted changes against the base, from a scan.
    pub(crate) fn changes(&self, scan: &Scan) -> Vec<Change> {
        manifest::diff(&self.manifest, &scan.actual)
    }

    /// The tree the working copy should hold for its state.
    pub(crate) fn expected(&self) -> Manifest {
        state::expected_tree(&self.state, &self.manifest)
    }

    /// Paths where the working tree differs from what the state says it should hold.
    pub(crate) fn dirty(&self, scan: &Scan) -> Vec<Vec<u8>> {
        manifest::diff(&self.expected(), &scan.actual)
            .into_iter()
            .map(|c| c.path)
            .collect()
    }

    /// Fails with exit 4 when the working tree has uncommitted changes.
    pub(crate) fn require_clean(&mut self, what: &str) -> Result<Scan> {
        let scan = self.scan()?;
        let dirty = self.dirty(&scan);
        if !dirty.is_empty() {
            return Err(WcError::dirty(
                format!(
                    "{what} refused: {} path(s) have uncommitted changes; commit them \
                     (`bee commit -m ...`), or undo them",
                    dirty.len()
                ),
                dirty.iter().map(|p| fsutil::path_text(p)).collect(),
            ));
        }
        Ok(scan)
    }

    /// `bee status`: uncommitted changes, the stack and any rebase in progress. Never
    /// touches the network.
    pub fn status(&mut self) -> Result<StatusReport> {
        let scan = self.scan()?;
        let changes = self.changes(&scan);
        let rebase = self.state.rebase.as_ref();
        Ok(StatusReport {
            root: self.root.display().to_string(),
            repo: self.config.repo.clone(),
            target_ref: self.config.target_ref.clone(),
            head: self.state.head.to_text(),
            onto: self.state.onto.to_text(),
            onto_version: self.state.onto_version,
            stack: report::stack_views(&self.state),
            clean: changes.is_empty() && rebase.is_none(),
            changes: report::file_changes(&changes),
            conflicts: report::conflict_views(rebase),
            rebase: report::rebase_view(rebase),
            sparse: self.state.sparse.display(),
            hidden: report::hidden_views(&self.state.hidden),
            skipped: scan.skipped,
            recovered: self.recovered.clone(),
        })
    }

    /// The `.gitattributes` rules of the working tree as it is on disk: every tracked
    /// `.gitattributes` plus `extra` paths (new ones about to be committed), parents first.
    pub(crate) fn worktree_attributes(&self, extra: &[Vec<u8>]) -> Result<crate::eol::Attributes> {
        let mut paths: Vec<Vec<u8>> = self
            .manifest
            .keys()
            .filter(|p| crate::eol::is_attributes_file(p))
            .cloned()
            .chain(
                extra
                    .iter()
                    .filter(|p| crate::eol::is_attributes_file(p))
                    .cloned(),
            )
            .collect();
        paths.sort_by_key(|p| (p.iter().filter(|&&b| b == b'/').count(), p.clone()));
        paths.dedup();
        let mut attrs = crate::eol::Attributes::new();
        for p in paths {
            if let Some(text) = fsutil::read_optional(&fsutil::fs_path(&self.root, &p)?)? {
                attrs.add_file(crate::eol::dir_of(&p), &text);
            }
        }
        Ok(attrs)
    }

    /// The `.gitattributes` rules of a tree about to be checked out: each one's bytes from
    /// disk when unchanged, else from the local object store or the server.
    fn checkout_attributes(
        &self,
        remote: &Remote<'_>,
        tree: &Manifest,
        targets: &[(PathBytes, Option<Entry>)],
    ) -> Result<crate::eol::Attributes> {
        let mut paths: Vec<(&Vec<u8>, &Entry)> = tree
            .iter()
            .filter(|(p, _)| crate::eol::is_attributes_file(p))
            .collect();
        paths.sort_by_key(|(p, _)| (p.iter().filter(|&&b| b == b'/').count(), (*p).clone()));
        let mut attrs = crate::eol::Attributes::new();
        if paths.is_empty() {
            return Ok(attrs);
        }
        let changing: BTreeSet<&Vec<u8>> = targets.iter().map(|(p, _)| &p.0).collect();
        let mut fetch = Vec::new();
        for (p, e) in &paths {
            if !changing.contains(p)
                && fsutil::read_optional(&fsutil::fs_path(&self.root, p)?)?.is_some()
            {
                continue;
            }
            if fsutil::read_optional(&self.layout.object(&e.id))?.is_none() {
                fetch.push(e.id);
            }
        }
        let fetched = if fetch.is_empty() {
            BTreeMap::new()
        } else {
            remote.blobs(&fetch)?
        };
        for (p, e) in paths {
            let bytes = if changing.contains(p) {
                match fsutil::read_optional(&self.layout.object(&e.id))? {
                    Some(b) => Some(b),
                    None => fetched.get(&e.id).and_then(|r| r.as_ref().ok()).cloned(),
                }
            } else {
                match fsutil::read_optional(&fsutil::fs_path(&self.root, p)?)? {
                    Some(b) => Some(b),
                    None => fetched.get(&e.id).and_then(|r| r.as_ref().ok()).cloned(),
                }
            };
            if let Some(text) = bytes {
                attrs.add_file(crate::eol::dir_of(p), &text);
            }
        }
        Ok(attrs)
    }

    /// Applies redactions and unredactions made since the working copy's redaction epoch
    /// (SPEC §13): a clean file whose blob is now redacted becomes the placeholder, a dirty
    /// one is reported and left alone, and an unredacted placeholder gets its content back.
    /// The state (hidden paths, epoch) is written before the files, so a crash in between
    /// leaves paths that the next refresh finishes.
    pub(crate) fn refresh_redactions(
        &mut self,
        t: &dyn Transport,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let remote = self.remote(t);
        let mut since = self.state.redaction_epoch + 1;
        let (mut records, mut current) = remote.redactions_since(since)?;
        if current < self.state.redaction_epoch {
            // The repo's epoch went backwards (a restore): start again from epoch 0.
            since = 0;
            (records, current) = remote.redactions_since(since)?;
        }
        let mut redacted: BTreeSet<ObjectId> = BTreeSet::new();
        let mut unredacted: BTreeSet<ObjectId> = BTreeSet::new();
        for (id, state) in &records {
            if *state == crate::remote::RedactionState::Unredacted {
                unredacted.insert(*id);
                redacted.remove(id);
            } else {
                redacted.insert(*id);
                unredacted.remove(id);
            }
        }
        let expected = self.expected();
        let attrs = self.worktree_attributes(&[])?;
        let hidden_redacted: BTreeSet<Vec<u8>> = self
            .state
            .hidden
            .iter()
            .filter(|h| h.reason == "redacted")
            .map(|h| h.path.0.clone())
            .collect();
        let mut to_hide = Vec::new();
        let mut dirty = Vec::new();
        let mut to_restore = Vec::new();
        for (path, entry) in &expected {
            if entry.mode == Mode::Gitlink || !self.state.sparse.contains(path) {
                continue;
            }
            let was_hidden = hidden_redacted.contains(path);
            if redacted.contains(&entry.id) || (was_hidden && !unredacted.contains(&entry.id)) {
                match checkout::placeholder_or_entry(&self.root, path, entry, &attrs)? {
                    Some(true) if was_hidden => {}
                    Some(_) => to_hide.push((path.clone(), *entry)),
                    None if was_hidden => {}
                    None => dirty.push(fsutil::path_text(path)),
                }
            } else if was_hidden && unredacted.contains(&entry.id) {
                to_restore.push((path.clone(), *entry));
            }
        }
        // State first: hidden paths and the epoch.
        let mut state = self.state.clone();
        for (path, _) in &to_hide {
            if !hidden_redacted.contains(path) {
                state.hidden.push(HiddenPath {
                    path: PathBytes(path.clone()),
                    reason: "redacted".into(),
                });
            }
        }
        state.redaction_epoch = current.max(if since == 0 { 0 } else { state.redaction_epoch });
        if state != self.state {
            fsutil::atomic_write(&self.layout.state(), &state::to_toml(&state)?)?;
            self.state = state;
        }
        let mut replaced = Vec::new();
        for (path, entry) in &to_hide {
            if let Some(st) = checkout::write_placeholder(&self.root, path)? {
                self.cache.entries.insert(path.clone(), (st, *entry));
            }
            replaced.push(fsutil::path_text(path));
        }
        if !to_restore.is_empty() {
            let ids: Vec<ObjectId> = to_restore.iter().map(|(_, e)| e.id).collect();
            let blobs = remote.blobs(&ids)?;
            let mut restored = BTreeSet::new();
            for (path, entry) in &to_restore {
                if let Some(Ok(bytes)) = blobs.get(&entry.id) {
                    if checkout::placeholder_or_entry(&self.root, path, entry, &attrs)?.is_some()
                        && let Some(st) =
                            checkout::write_entry(&self.root, path, entry, bytes, &attrs)?
                    {
                        self.cache.entries.insert(path.clone(), (st, *entry));
                    }
                    restored.insert(path.clone());
                }
            }
            let mut state = self.state.clone();
            state.hidden.retain(|h| !restored.contains(&h.path.0));
            fsutil::atomic_write(&self.layout.state(), &state::to_toml(&state)?)?;
            self.state = state;
        }
        if !replaced.is_empty() || !to_restore.is_empty() {
            fsutil::atomic_write(&self.layout.statcache(), &self.cache.encode())?;
        }
        Ok((replaced, dirty))
    }

    /// Commits new local state through the journal (see [`crate::journal`]).
    pub(crate) fn commit_local(&mut self, t: &dyn Transport, mut c: Commit) -> Result<OpReport> {
        let (redacted, redacted_dirty) = self.refresh_redactions(t)?;
        c.state.redaction_epoch = self.state.redaction_epoch;
        let seq = journal::next_seq(&self.layout)?;
        let manifest_undo: Vec<(PathBytes, Option<Entry>)> =
            manifest::diff(&self.manifest, &c.manifest)
                .into_iter()
                .map(|ch| (PathBytes(ch.path), ch.old))
                .collect();
        let record = OpRecord {
            seq,
            op: c.op.to_owned(),
            summary: c.summary,
            before: self.state.clone(),
            after: c.state,
            manifest_undo,
            soft: c.soft,
            undoable: c.undoable,
            undoes: c.undoes,
        };
        fsutil::atomic_write(&self.layout.manifest_next(), &manifest::encode(&c.manifest))?;
        let pending = Pending {
            record,
            targets: c.targets,
        };
        journal::write_pending(&self.layout, &pending)?;
        if let Some(config) = self.pending_config.take() {
            fsutil::atomic_write(&self.layout.config(), &state::to_json(&config)?)?;
        }
        self.faults.hit("journal")?;
        let applied = self.finish(t, pending)?;
        let mut report = self.op_report(c.op, seq, &applied);
        report.redacted = redacted;
        report.redacted_dirty = redacted_dirty;
        Ok(report)
    }

    /// Steps 3 to 5 of the journal protocol; idempotent, so recovery runs it again.
    fn finish(&mut self, t: &dyn Transport, mut pending: Pending) -> Result<Applied> {
        let next_manifest = match fsutil::read_optional(&self.layout.manifest_next())? {
            Some(b) => manifest::decode(&b)?,
            // Already installed by an earlier attempt.
            None => match fsutil::read_optional(&self.layout.manifest())? {
                Some(b) => manifest::decode(&b)?,
                None => Manifest::new(),
            },
        };
        let after = &pending.record.after;
        let expected = state::expected_tree(after, &next_manifest);
        let skip = if checkout::case_insensitive(&self.layout.dir) {
            checkout::case_collisions(&expected)
        } else {
            BTreeSet::new()
        };
        let remote = Remote::new(t, &self.config.repo);
        let attrs = self.checkout_attributes(&remote, &expected, &pending.targets)?;
        let mut fetch = |ids: &[ObjectId]| remote.blobs(ids);
        let applied = checkout::apply(
            &self.root,
            &self.layout,
            &pending.targets,
            &skip,
            &mut fetch,
            &self.faults,
            &attrs,
        )?;
        // Hidden paths: the old ones the targets did not touch, plus what this apply hid.
        let touched: BTreeSet<&Vec<u8>> = pending.targets.iter().map(|(p, _)| &p.0).collect();
        let mut hidden: BTreeMap<Vec<u8>, HiddenPath> = BTreeMap::new();
        for h in &pending.record.before.hidden {
            if !touched.contains(&h.path.0) && expected.contains_key(&h.path.0) {
                hidden.insert(h.path.0.clone(), h.clone());
            }
        }
        for h in &applied.hidden {
            hidden.insert(h.path.0.clone(), h.clone());
        }
        pending.record.after.hidden = hidden.into_values().collect();
        if self.layout.manifest_next().exists() {
            fs::rename(self.layout.manifest_next(), self.layout.manifest()).map_err(|e| {
                WcError::io(format!("install {}", self.layout.manifest().display()), &e)
            })?;
            fsutil::fsync_dir(&self.layout.dir)?;
        }
        self.faults.hit("manifest")?;
        fsutil::atomic_write(
            &self.layout.state(),
            &state::to_toml(&pending.record.after)?,
        )?;
        self.faults.hit("state")?;
        journal::append_oplog(&self.layout, &pending.record)?;
        journal::clear_pending(&self.layout)?;
        write_conflicts(&self.layout, &pending.record.after)?;
        self.state = pending.record.after.clone();
        self.manifest = next_manifest;
        for (p, st, e) in &applied.stats {
            self.cache.entries.insert(p.clone(), (*st, *e));
        }
        for (p, e) in &pending.targets {
            if e.is_none() {
                self.cache.entries.remove(&p.0);
            }
        }
        fsutil::atomic_write(&self.layout.statcache(), &self.cache.encode())?;
        Ok(applied)
    }

    pub(crate) fn op_report(&self, op: &str, seq: u64, applied: &Applied) -> OpReport {
        let rebase = self.state.rebase.as_ref();
        OpReport {
            op: op.to_owned(),
            seq,
            summary: String::new(),
            head: self.state.head.to_text(),
            onto: self.state.onto.to_text(),
            stack: report::stack_views(&self.state),
            changeset: None,
            change_id: None,
            written: applied.written,
            removed: applied.removed,
            conflicts: report::conflict_views(rebase),
            rebase: report::rebase_view(rebase),
            dropped: rebase.map(|r| r.dropped.clone()).unwrap_or_default(),
            hidden: report::hidden_views(&self.state.hidden),
            land: None,
            undid: None,
            redacted: Vec::new(),
            redacted_dirty: Vec::new(),
        }
    }

    /// A report for an operation that changed nothing.
    pub(crate) fn noop_report(&self, op: &str, summary: &str) -> OpReport {
        let mut r = self.op_report(op, 0, &Applied::default());
        r.summary = summary.to_owned();
        r
    }
}

/// Makes `.bee/conflicts/` hold exactly one file per conflicted path of the rebase in
/// progress: `<first 16 hex digits of BLAKE3(path)>.toml` with the path, the kind, the base,
/// ours, theirs and rendered entries, and the markers and resolved flags. Derived from the
/// state, so a crash between the two is repaired by the next write or open.
pub(crate) fn write_conflicts(layout: &Layout, state: &State) -> Result<()> {
    let dir = layout.conflicts();
    fs::create_dir_all(&dir).map_err(|e| WcError::io(format!("create {}", dir.display()), &e))?;
    let mut want: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    if let Some(op) = state.rebase.as_ref().filter(|op| op.current.is_some()) {
        for c in &op.conflicts {
            let name = format!(
                "{}.toml",
                bee_core::hex::encode(&bee_object::digest(&c.path.0)[..8])
            );
            let text =
                toml::to_string(c).map_err(|e| WcError::internal("encode", e.to_string()))?;
            want.insert(name, text.into_bytes());
        }
    }
    let read =
        fs::read_dir(&dir).map_err(|e| WcError::io(format!("read {}", dir.display()), &e))?;
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if !want.contains_key(&name) {
            let _ = fs::remove_file(item.path());
        }
    }
    for (name, bytes) in want {
        let path = dir.join(&name);
        if fsutil::read_optional(&path)?.as_deref() != Some(bytes.as_slice()) {
            fsutil::atomic_write(&path, &bytes)?;
        }
    }
    Ok(())
}

/// Targets that turn working tree `from` into `to`.
pub(crate) fn targets_between(from: &Manifest, to: &Manifest) -> Vec<(PathBytes, Option<Entry>)> {
    manifest::diff(from, to)
        .into_iter()
        .map(|c| (PathBytes(c.path), c.new))
        .collect()
}
