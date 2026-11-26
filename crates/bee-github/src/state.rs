//! The local state directory that makes an import resumable.
//!
//! ```text
//! <state>/
//!   state.json      small checkpoint: phase, claim secret, live-sync cursor, ETags
//!   journal.jsonl   append-only log of fetched pages, cursors, finished steps, applied records
//!   mirror.git/     bare mirror of the GitHub repository (git phase)
//!   wiki.git/       bare mirror of the wiki, when there is one
//!   report.json     the last fidelity report
//!   redirects.json  the cutover redirect map
//!   policy.toml     the generated land rules, for review
//! ```
//!
//! Every fetched page is appended to the journal before the step's cursor moves, and every
//! record placed in the target is journaled with its outcome right after the target accepted
//! it. A killed import therefore resumes by replaying the journal: pages are merged again
//! (idempotent, keyed maps), finished steps are skipped, a step resumes at its saved cursor,
//! and records already placed are not placed twice. A line cut short by a crash is ignored.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, io};
use crate::report::Outcome;

/// Where the import is (SPEC §17.5 phases plus the steps of the first run).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Nothing done yet.
    #[default]
    New,
    /// Git history mirrored and imported.
    Git,
    /// Metadata fetched.
    Metadata,
    /// Metadata placed in the target; mirror phase reached.
    Mirror,
    /// Live sync running (webhooks and polling).
    Live,
    /// Frozen for cutover: GitHub writes are no longer followed.
    Frozen,
    /// Cut over: Beehive is the source of truth.
    CutOver,
}

impl Phase {
    /// The report's name for the phase.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::New => "new",
            Phase::Git => "git",
            Phase::Metadata => "metadata",
            Phase::Mirror => "mirror",
            Phase::Live => "live",
            Phase::Frozen => "frozen",
            Phase::CutOver => "cut_over",
        }
    }
}

/// Live-sync position.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveCursor {
    /// Newest `updated_at` seen by polling (the next poll's `since`).
    pub since: Option<String>,
    /// Recent webhook delivery ids, oldest first (duplicates are ignored).
    pub deliveries: VecDeque<String>,
}

/// How many delivery ids [`LiveCursor`] remembers.
pub const DELIVERY_MEMORY: usize = 10_000;

/// The small checkpoint (`state.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Format version.
    pub version: u32,
    /// `owner/repo`.
    pub source: String,
    /// Phase reached.
    pub phase: Phase,
    /// Secret the claim tokens are derived from (hex). Generated once per state directory.
    pub claim_secret: String,
    /// Claims made: GitHub user id → principal id.
    pub claims: BTreeMap<u64, String>,
    /// Live-sync position.
    pub live: LiveCursor,
    /// Stored ETags and bodies by URL.
    pub etags: BTreeMap<String, (String, String)>,
}

/// One journal line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Entry {
    /// Raw items of one page of `step`.
    Items {
        /// The step (`issues`, `reviews:12`, ...).
        step: String,
        /// The items as GitHub sent them.
        items: Vec<Value>,
    },
    /// The next page of `step` (none: the list ended).
    Cursor {
        /// The step.
        step: String,
        /// Next page URL.
        next: Option<String>,
    },
    /// `step` finished.
    Done {
        /// The step.
        step: String,
    },
    /// A record was placed in the target with these outcomes.
    Applied {
        /// The record key (`pr:12`, `issue:7`, `label:bug`, ...).
        key: String,
        /// Fingerprint of the source record when it was placed; a changed record is placed
        /// again (live sync, re-runs).
        fingerprint: String,
        /// What happened to it and its parts.
        outcomes: Vec<Outcome>,
    },
}

/// The journal replayed.
#[derive(Clone, Debug, Default)]
pub struct Replay {
    /// Page items in journal order.
    pub items: Vec<(String, Vec<Value>)>,
    /// Latest cursor per step.
    pub cursors: BTreeMap<String, Option<String>>,
    /// Finished steps.
    pub done: BTreeSet<String>,
    /// Placed records: key → (fingerprint, outcomes), latest wins.
    pub applied: BTreeMap<String, (String, Vec<Outcome>)>,
}

/// A state directory.
#[derive(Clone, Debug)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// Opens (creating) a state directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<StateDir> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| io("creating the state directory", e))?;
        Ok(StateDir { root })
    }

    /// The directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A path inside the directory.
    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The git mirror.
    pub fn mirror(&self) -> PathBuf {
        self.path("mirror.git")
    }

    /// The wiki mirror.
    pub fn wiki(&self) -> PathBuf {
        self.path("wiki.git")
    }

    /// Loads `state.json` (default when absent).
    pub fn load(&self) -> Result<Checkpoint> {
        match std::fs::read(self.path("state.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| {
                crate::error::invalid(
                    "invalid_state",
                    format!(
                        "{} is not a bee-github state file: {e}",
                        self.root.display()
                    ),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Checkpoint {
                version: 1,
                ..Checkpoint::default()
            }),
            Err(e) => Err(io("reading state.json", e)),
        }
    }

    /// Writes `state.json` atomically.
    pub fn save(&self, cp: &Checkpoint) -> Result<()> {
        self.write_atomic(
            "state.json",
            &serde_json::to_vec_pretty(cp).unwrap_or_default(),
        )
    }

    /// Writes a file atomically (temporary file, then rename).
    pub fn write_atomic(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let tmp = self.path(&format!(".{name}.tmp"));
        std::fs::write(&tmp, bytes).map_err(|e| io(&format!("writing {name}"), e))?;
        std::fs::rename(&tmp, self.path(name)).map_err(|e| io(&format!("replacing {name}"), e))
    }

    /// Appends journal lines.
    pub fn append(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::new();
        for e in entries {
            serde_json::to_writer(&mut buf, e).unwrap_or_default();
            buf.push(b'\n');
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path("journal.jsonl"))
            .map_err(|e| io("opening the journal", e))?;
        f.write_all(&buf)
            .and_then(|_| f.flush())
            .map_err(|e| io("appending to the journal", e))
    }

    /// Reads the journal. A last line cut short by a crash is ignored; any other unreadable
    /// line is an error (the journal is not ours or was edited).
    pub fn replay(&self) -> Result<Replay> {
        let bytes = match std::fs::read(self.path("journal.jsonl")) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Replay::default()),
            Err(e) => return Err(io("reading the journal", e)),
        };
        let mut r = Replay::default();
        let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
        let last = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: Entry = match serde_json::from_slice(line) {
                Ok(e) => e,
                Err(_) if i == last => break,
                Err(e) => {
                    return Err(crate::error::invalid(
                        "invalid_state",
                        format!("journal line {} is unreadable: {e}", i + 1),
                    ));
                }
            };
            match entry {
                Entry::Items { step, items } => r.items.push((step, items)),
                Entry::Cursor { step, next } => {
                    r.cursors.insert(step, next);
                }
                Entry::Done { step } => {
                    r.done.insert(step);
                }
                Entry::Applied {
                    key,
                    fingerprint,
                    outcomes,
                } => {
                    r.applied.insert(key, (fingerprint, outcomes));
                }
            }
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_survives_a_torn_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateDir::open(dir.path()).unwrap();
        s.append(&[
            Entry::Items {
                step: "issues".into(),
                items: vec![serde_json::json!({"number": 1})],
            },
            Entry::Cursor {
                step: "issues".into(),
                next: Some("u2".into()),
            },
            Entry::Done {
                step: "labels".into(),
            },
        ])
        .unwrap();
        // A crash in the middle of a write.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(s.path("journal.jsonl"))
            .unwrap();
        f.write_all(br#"{"k":"items","step":"iss"#).unwrap();
        let r = s.replay().unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.cursors["issues"].as_deref(), Some("u2"));
        assert!(r.done.contains("labels"));
    }

    #[test]
    fn checkpoint_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateDir::open(dir.path()).unwrap();
        let mut cp = s.load().unwrap();
        assert_eq!(cp.phase, Phase::New);
        cp.phase = Phase::Live;
        cp.claims.insert(5, "01P".into());
        s.save(&cp).unwrap();
        assert_eq!(s.load().unwrap(), cp);
    }
}
