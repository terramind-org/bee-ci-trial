//! Where git history goes: [`GitTarget`]. [`StoreTarget`] imports a mirror straight into
//! bee stores with `bee-import` (in-process: the server, tests);
//! [`crate::server::ServerTarget`] asks a running `bee-server` to do it over its
//! `ImportService`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bee_core::ObjectId;
use bee_git::GitOid;
use bee_import::report::{Category, ReportOptions};
use bee_import::{DerivedHook, ImportOptions, ImportReport, ImportTarget};

use crate::error::Result;

/// Which repository of a GitHub repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitRepo {
    /// The repository itself.
    Main,
    /// Its wiki.
    Wiki,
}

/// How to import git history.
#[derive(Clone, Debug)]
pub struct GitOptions {
    /// Report label (`github:acme/api`).
    pub source: String,
    /// Read every changeset back and export it, comparing ids (SPEC §17.6).
    pub verify: bool,
    /// Time for rows and ref-log entries (epoch ms).
    pub at_ms: u64,
}

/// A destination for git history.
#[async_trait]
pub trait GitTarget: Send + Sync {
    /// Imports the bare repository at `mirror`. `Ok(None)` when the target has no place
    /// for this repository (a wiki without a wiki repo).
    async fn import(
        &self,
        mirror: &Path,
        repo: GitRepo,
        opts: &GitOptions,
    ) -> Result<Option<ImportReport>>;

    /// The bee changeset a git commit was imported as, when the target can say.
    async fn changeset(&self, sha: &str) -> Result<Option<ObjectId>>;
}

/// Imports into bee stores directly.
#[derive(Clone)]
pub struct StoreTarget {
    /// The repository.
    pub main: ImportTarget,
    /// The wiki repository, when there is one.
    pub wiki: Option<ImportTarget>,
    /// The service principal recorded as creator.
    pub creator: String,
    /// Eager derived rows (the server's registry).
    pub derived: Option<Arc<dyn DerivedHook>>,
}

#[async_trait]
impl GitTarget for StoreTarget {
    async fn import(
        &self,
        mirror: &Path,
        repo: GitRepo,
        opts: &GitOptions,
    ) -> Result<Option<ImportReport>> {
        let target = match repo {
            GitRepo::Main => &self.main,
            GitRepo::Wiki => match &self.wiki {
                Some(w) => w,
                None => return Ok(None),
            },
        };
        let mut o = ImportOptions::new(self.creator.clone());
        o.source = match repo {
            GitRepo::Main => opts.source.clone(),
            GitRepo::Wiki => format!("{}.wiki", opts.source),
        };
        o.include_pull_refs = repo == GitRepo::Main;
        o.verify = opts.verify;
        o.at_ms = opts.at_ms;
        o.derived = self.derived.clone();
        let out = bee_import::import_repository(mirror, target, &o).await?;
        Ok(Some(out.report))
    }

    async fn changeset(&self, sha: &str) -> Result<Option<ObjectId>> {
        let Ok(oid) = GitOid::parse(sha) else {
            return Ok(None);
        };
        bee_import::lookup_bee_id(self.main.writer.meta().as_ref(), self.main.repo, &oid).await
    }
}

/// Adds `b` (a later run over the same mirror) to `a`: object counts and write stats are
/// summed, ref counts are the latest run's, lists are merged (refs by bee name, latest
/// wins), verification results are combined.
pub fn merge_reports(a: Option<ImportReport>, b: ImportReport) -> ImportReport {
    let Some(mut a) = a else { return b };
    for cb in b.categories {
        // Refs and annotated tags describe the whole ref set on every run: the latest run
        // wins. Objects are counted when first written, so they add up.
        let whole_set = matches!(cb.category.as_str(), "refs" | "annotated_tags");
        match a.categories.iter_mut().find(|c| c.category == cb.category) {
            Some(ca) if whole_set => *ca = cb,
            Some(ca) => {
                ca.imported += cb.imported;
                ca.archived += cb.archived;
                ca.excluded = ca.excluded.max(cb.excluded);
                for d in cb.drops {
                    if !ca.drops.contains(&d) {
                        ca.drops.push(d);
                    }
                }
                ca.drops.sort();
                ca.dropped = ca.drops.len() as u64;
            }
            None => a.categories.push(Category { ..cb }),
        }
    }
    for r in b.refs {
        a.refs.retain(|x| x.bee != r.bee);
        a.refs.push(r);
    }
    a.refs.sort_by(|x, y| x.bee.cmp(&y.bee));
    for r in b.renamed_refs {
        if !a.renamed_refs.iter().any(|x| x.git_hex == r.git_hex) {
            a.renamed_refs.push(r);
        }
    }
    a.renamed_refs.sort_by(|x, y| x.git_hex.cmp(&y.git_hex));
    a.lfs_objects.extend(b.lfs_objects);
    a.lfs_objects.sort();
    a.lfs_objects.dedup();
    let (va, vb) = (&mut a.verification, b.verification);
    va.performed |= vb.performed;
    va.commits_checked += vb.commits_checked;
    va.refs_checked = vb.refs_checked;
    va.commit_id_mismatches.extend(vb.commit_id_mismatches);
    va.graph_mismatches.extend(vb.graph_mismatches);
    va.ref_mismatches.extend(vb.ref_mismatches);
    let (sa, sb) = (&mut a.stats, b.stats);
    sa.commits_walked += sb.commits_walked;
    sa.commits_already_imported = sb.commits_already_imported;
    sa.changesets_written += sb.changesets_written;
    sa.objects_written += sb.objects_written;
    sa.bytes_written += sb.bytes_written;
    sa.refs_written += sb.refs_written;
    sa.records_written += sb.records_written;
    if b.default_branch.is_some() {
        a.default_branch = b.default_branch;
    }
    a.object_format = b.object_format;
    a.options.include_pull_refs |= b.options.include_pull_refs;
    a.options.verify |= b.options.verify;
    a
}

/// An empty git report (dry runs, metadata-only runs).
pub fn empty_report(source: &str) -> ImportReport {
    ImportReport::new(
        source,
        "sha1",
        ReportOptions {
            include_pull_refs: true,
            verify: false,
        },
    )
}
