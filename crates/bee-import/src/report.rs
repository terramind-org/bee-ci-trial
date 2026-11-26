//! The fidelity report (SPEC §17.6): what an import carried, archived and dropped, with a
//! reason for every dropped item. Serialized as JSON matching
//! `schemas/v1/import-report.schema.json` ([`ImportReport::to_json`]).
//!
//! The report is a pure function of the import's inputs: it holds no timestamps or
//! durations, so the same source imported into the same state gives the same bytes.

use serde::{Deserialize, Serialize};

/// The `schema` value of every report.
pub const REPORT_SCHEMA: &str = "bee.v1.import-report";

/// Stable drop reasons.
pub mod reason {
    /// A ref name bee cannot represent even after escaping.
    pub const UNSUPPORTED_REF_NAME: &str = "unsupported_ref_name";
    /// Two refs mapped to the same bee name and the fallback was taken too.
    pub const REF_NAME_COLLISION: &str = "ref_name_collision";
    /// A commit with more than 16 distinct parents (SPEC §4.6).
    pub const TOO_MANY_PARENTS: &str = "too_many_parents";
    /// A commit message over 1 MiB (SPEC §4.6).
    pub const MESSAGE_TOO_LARGE: &str = "message_too_large";
    /// A blob over the repository's blob size limit (SPEC §4.6).
    pub const BLOB_TOO_LARGE: &str = "blob_too_large";
    /// Another SPEC §4.6 limit (tree entries, delta entries, encoded size).
    pub const TOO_LARGE: &str = "too_large";
    /// Trees nested deeper than 256 levels (SPEC §4.5).
    pub const DEPTH_LIMIT: &str = "depth_limit";
    /// A path longer than 4096 bytes (SPEC §4.5).
    pub const INVALID_PATH: &str = "invalid_path";
    /// A tree entry name bee forbids (`.bee`, over 255 bytes; SPEC §4.2).
    pub const UNSUPPORTED_NAME: &str = "unsupported_name";
    /// A git tree with two entries of one name.
    pub const DUPLICATE_ENTRY: &str = "duplicate_entry";
    /// A git mode with no bee equivalent.
    pub const INVALID_MODE: &str = "invalid_mode";
    /// Bytes that do not hash to their id, a SHA-1 collision attack, or an object that would
    /// not reproduce its git id (`INTEGRITY`).
    pub const INTEGRITY: &str = "integrity";
    /// An object the source does not have.
    pub const MISSING_OBJECT: &str = "missing_object";
    /// A commit at the edge of a shallow clone: its parents are not in the source.
    pub const SHALLOW_BOUNDARY: &str = "shallow_boundary";
    /// A commit whose parent was dropped, so it cannot keep its git id.
    pub const PARENT_DROPPED: &str = "parent_dropped";
    /// A ref whose target commit was dropped.
    pub const TARGET_DROPPED: &str = "target_dropped";
    /// A tag (or ref) that points at a tree or blob rather than a commit.
    pub const TARGET_NOT_COMMIT: &str = "target_not_commit";
    /// Malformed git data bee cannot parse.
    pub const MALFORMED: &str = "malformed";
}

/// Report categories, in report order.
pub mod category {
    /// Commits (changesets with identical git ids).
    pub const COMMITS: &str = "commits";
    /// Trees.
    pub const TREES: &str = "trees";
    /// Blobs.
    pub const BLOBS: &str = "blobs";
    /// Refs (branches, tags, pull refs, others).
    pub const REFS: &str = "refs";
    /// Annotated tag objects, kept byte-exact as `git_tag` records.
    pub const TAGS: &str = "annotated_tags";
    /// Submodule pointers (gitlinks).
    pub const SUBMODULES: &str = "submodules";
    /// Git LFS pointer files (the large objects are fetched by the forge importer).
    pub const LFS: &str = "lfs_pointers";
    /// Every category, in order.
    pub const ALL: [&str; 7] = [COMMITS, TREES, BLOBS, REFS, TAGS, SUBMODULES, LFS];
}

/// One dropped item.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DroppedItem {
    /// What was dropped: a git object id or a git ref name.
    pub item: String,
    /// A stable reason from [`reason`].
    pub reason: String,
    /// Human-readable detail (the SPEC §12.2 error message, or the missing parent).
    pub detail: String,
}

/// Counts for one category.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    /// The category name (see [`category`]).
    pub category: String,
    /// Items carried into bee by this run.
    pub imported: u64,
    /// Items kept as read-only records.
    pub archived: u64,
    /// Items that could not be carried; one entry each in `drops`.
    pub dropped: u64,
    /// Items left out on purpose by an option (not a loss).
    pub excluded: u64,
    /// Every dropped item with its reason, sorted.
    pub drops: Vec<DroppedItem>,
}

/// A ref whose bee name differs from the plain prefix mapping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamedRef {
    /// The git ref name (lossy UTF-8; bytes that are not UTF-8 appear as U+FFFD).
    pub git: String,
    /// The git ref name as lowercase hex bytes, exact.
    pub git_hex: String,
    /// The bee ref name.
    pub bee: String,
    /// Why it was renamed.
    pub reason: String,
}

/// What happened to one ref.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefRecord {
    /// The git ref name (lossy UTF-8).
    pub git: String,
    /// The bee ref name.
    pub bee: String,
    /// `branch`, `tag`, `pull_request` or `other`.
    pub kind: String,
    /// The object the git ref points at (a commit, or an annotated tag).
    pub git_target: String,
    /// The commit it resolves to after peeling tags.
    pub commit: String,
    /// The bee changeset the ref now targets.
    pub changeset: String,
    /// Annotated tag objects peeled on the way, outermost first.
    pub tags: Vec<String>,
    /// `created`, `updated` or `unchanged`.
    pub action: String,
    /// The ref version after the import.
    pub version: u64,
}

/// One LFS pointer found in imported history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LfsObject {
    /// The LFS object id (`sha256:<hex>`).
    pub oid: String,
    /// The object size the pointer declares.
    pub size: u64,
}

/// Round-trip verification (the importer's `verify` option): every imported changeset is
/// read back from the stores and exported to git with a fresh exporter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    /// True when verification ran.
    pub performed: bool,
    /// Commits exported and compared.
    pub commits_checked: u64,
    /// Refs compared.
    pub refs_checked: u64,
    /// Commits whose exported id differs: `<original> -> <exported>`.
    pub commit_id_mismatches: Vec<String>,
    /// Commits whose changeset parents are not the git parents.
    pub graph_mismatches: Vec<String>,
    /// Refs whose bee target does not export to the git target.
    pub ref_mismatches: Vec<String>,
}

impl Verification {
    /// True when nothing mismatched.
    pub fn passed(&self) -> bool {
        self.commit_id_mismatches.is_empty()
            && self.graph_mismatches.is_empty()
            && self.ref_mismatches.is_empty()
    }
}

/// Work counters for this run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Commits reachable from the refs that were walked (new ones plus known boundaries).
    pub commits_walked: u64,
    /// Commits already imported by an earlier run, where the walk stopped.
    pub commits_already_imported: u64,
    /// Changeset rows inserted.
    pub changesets_written: u64,
    /// Object rows inserted (blobs, trees, metas).
    pub objects_written: u64,
    /// Bytes uploaded (object rows inserted, by encoded size).
    pub bytes_written: u64,
    /// Ref updates written.
    pub refs_written: u64,
    /// Derived rows written outside changeset transactions (`git_tag` records).
    pub records_written: u64,
}

/// Options the import ran with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportOptions {
    /// `refs/pull/*` were imported.
    pub include_pull_refs: bool,
    /// Round-trip verification was requested.
    pub verify: bool,
}

/// The fidelity report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// Always [`REPORT_SCHEMA`].
    pub schema: String,
    /// Where the history came from (a path, `github:acme/api`, ...).
    pub source: String,
    /// `sha1` or `sha256`.
    pub object_format: String,
    /// The branch the source's `HEAD` named, mapped to bee, when known.
    pub default_branch: Option<String>,
    /// True only when nothing was dropped and verification (if requested) passed: the one
    /// condition under which "imported 100%" may be said.
    pub complete: bool,
    /// One line: `imported 100%` or what was lost.
    pub summary: String,
    /// The options.
    pub options: ReportOptions,
    /// Per-category counts, in [`category::ALL`] order.
    pub categories: Vec<Category>,
    /// Refs whose names were changed to fit bee's grammar.
    pub renamed_refs: Vec<RenamedRef>,
    /// Every carried ref, sorted by bee name.
    pub refs: Vec<RefRecord>,
    /// Distinct LFS pointers (the forge importer fetches these objects).
    pub lfs_objects: Vec<LfsObject>,
    /// Round-trip verification.
    pub verification: Verification,
    /// Work counters.
    pub stats: Stats,
}

impl ImportReport {
    /// An empty report for a source.
    pub fn new(source: impl Into<String>, object_format: &str, options: ReportOptions) -> Self {
        ImportReport {
            schema: REPORT_SCHEMA.to_string(),
            source: source.into(),
            object_format: object_format.to_string(),
            default_branch: None,
            complete: true,
            summary: String::new(),
            options,
            categories: category::ALL
                .iter()
                .map(|c| Category {
                    category: (*c).to_string(),
                    ..Category::default()
                })
                .collect(),
            renamed_refs: Vec::new(),
            refs: Vec::new(),
            lfs_objects: Vec::new(),
            verification: Verification::default(),
            stats: Stats::default(),
        }
    }

    /// The category with this name (one of [`category::ALL`]).
    pub fn category(&self, name: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.category == name)
    }

    pub(crate) fn category_mut(&mut self, name: &str) -> &mut Category {
        if let Some(i) = self.categories.iter().position(|c| c.category == name) {
            return &mut self.categories[i];
        }
        self.categories.push(Category {
            category: name.to_string(),
            ..Category::default()
        });
        let last = self.categories.len() - 1;
        &mut self.categories[last]
    }

    pub(crate) fn drop_item(
        &mut self,
        cat: &str,
        item: impl Into<String>,
        reason: &str,
        detail: impl Into<String>,
    ) {
        let c = self.category_mut(cat);
        c.dropped += 1;
        c.drops.push(DroppedItem {
            item: item.into(),
            reason: reason.to_string(),
            detail: detail.into(),
        });
    }

    /// Every dropped item across categories.
    pub fn drops(&self) -> impl Iterator<Item = &DroppedItem> {
        self.categories.iter().flat_map(|c| c.drops.iter())
    }

    /// Total dropped items.
    pub fn dropped_total(&self) -> u64 {
        self.categories.iter().map(|c| c.dropped).sum()
    }

    /// Sorts the lists and computes `complete` and `summary`.
    pub(crate) fn finish(&mut self) {
        for c in &mut self.categories {
            c.drops.sort();
        }
        self.renamed_refs.sort_by(|a, b| a.git_hex.cmp(&b.git_hex));
        self.refs.sort_by(|a, b| a.bee.cmp(&b.bee));
        self.lfs_objects.sort();
        self.lfs_objects.dedup();
        let dropped = self.dropped_total();
        let verified = !self.verification.performed || self.verification.passed();
        self.complete = dropped == 0 && verified;
        self.summary = if self.complete {
            "imported 100%".to_string()
        } else {
            let mut parts = Vec::new();
            for c in &self.categories {
                if c.dropped > 0 {
                    parts.push(format!("{} {} dropped", c.dropped, c.category));
                }
            }
            if !verified {
                parts.push("round-trip verification failed".to_string());
            }
            format!("incomplete: {}", parts.join(", "))
        };
    }

    /// The report as pretty JSON (stable field order).
    pub fn to_json(&self) -> String {
        // Serializing plain structs of strings and integers cannot fail.
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Parses a report.
    pub fn from_json(text: &str) -> serde_json::Result<ImportReport> {
        serde_json::from_str(text)
    }
}
