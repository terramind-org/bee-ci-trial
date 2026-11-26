//! The GitHub fidelity report (SPEC §17.6): the git importer's report
//! ([`bee_import::ImportReport`]) plus one category per GitHub entity and a `github`
//! section (placeholders, manual steps, untranslatable settings, redirects).
//!
//! Every record the importer handles ends as an [`Outcome`]: imported, archived (kept as a
//! read-only record or in a degraded form, with a note), excluded on purpose, or dropped with
//! a stable reason and a detail. The report is built from outcomes alone, so a dropped item
//! cannot go unexplained: `unexplained_drops` counts drops without a reason and is zero by
//! construction. Like the git report, it holds no times or durations.

use bee_import::ImportReport;
use bee_import::report::{Category, DroppedItem};
use serde::{Deserialize, Serialize};

/// GitHub categories, in report order (after the git importer's).
pub mod category {
    /// Pull requests → change requests with the same number.
    pub const PULL_REQUESTS: &str = "pull_requests";
    /// PR pushes and force-pushes → CR revisions.
    pub const PR_REVISIONS: &str = "pr_revisions";
    /// Commits a force-push replaced, fetched while GitHub still serves them.
    pub const FORCE_PUSH_COMMITS: &str = "force_push_commits";
    /// Submitted reviews.
    pub const REVIEWS: &str = "reviews";
    /// Review (line and file) comments with their anchors.
    pub const REVIEW_COMMENTS: &str = "review_comments";
    /// Conversation comments on PRs and issues.
    pub const ISSUE_COMMENTS: &str = "issue_comments";
    /// Issues with the same number.
    pub const ISSUES: &str = "issues";
    /// Label definitions.
    pub const LABELS: &str = "labels";
    /// Milestones.
    pub const MILESTONES: &str = "milestones";
    /// Per-user reactions.
    pub const REACTIONS: &str = "reactions";
    /// Releases.
    pub const RELEASES: &str = "releases";
    /// Release assets.
    pub const RELEASE_ASSETS: &str = "release_assets";
    /// Git LFS objects named by pointer files.
    pub const LFS_OBJECTS: &str = "lfs_objects";
    /// Branch protection → land rules.
    pub const BRANCH_PROTECTION: &str = "branch_protection";
    /// CODEOWNERS → owners.
    pub const CODEOWNERS: &str = "codeowners";
    /// Webhooks (pre-filled, disabled; secrets must be re-entered).
    pub const WEBHOOKS: &str = "webhooks";
    /// GitHub Actions workflows (kept as-is in git; bee-workflow runs them).
    pub const WORKFLOWS: &str = "workflows";
    /// The wiki repository.
    pub const WIKI: &str = "wiki";
    /// GitHub accounts → placeholder principals.
    pub const PRINCIPALS: &str = "principals";
    /// Actions secrets (names only; values are not exportable).
    pub const SECRETS: &str = "secrets";
    /// Deploy keys (titles only).
    pub const DEPLOY_KEYS: &str = "deploy_keys";
    /// Every GitHub category, in order.
    pub const ALL: [&str; 21] = [
        PULL_REQUESTS,
        PR_REVISIONS,
        FORCE_PUSH_COMMITS,
        REVIEWS,
        REVIEW_COMMENTS,
        ISSUE_COMMENTS,
        ISSUES,
        LABELS,
        MILESTONES,
        REACTIONS,
        RELEASES,
        RELEASE_ASSETS,
        LFS_OBJECTS,
        BRANCH_PROTECTION,
        CODEOWNERS,
        WEBHOOKS,
        WORKFLOWS,
        WIKI,
        PRINCIPALS,
        SECRETS,
        DEPLOY_KEYS,
    ];
}

/// Stable drop reasons added by the GitHub importer (the git importer's are in
/// [`bee_import::report::reason`]).
pub mod reason {
    /// A force-push replaced commits GitHub no longer serves.
    pub const FORCE_PUSH_COMMIT_UNREACHABLE: &str = "force_push_commit_unreachable";
    /// A PR head or revision commit is not in the imported history.
    pub const MISSING_COMMIT: &str = "missing_commit";
    /// The target already has a different record with this number.
    pub const NUMBER_TAKEN: &str = "number_taken";
    /// The target cannot store this kind of record yet.
    pub const NOT_SUPPORTED_BY_TARGET: &str = "not_supported_by_target";
    /// The credential cannot read this section on GitHub.
    pub const UNREADABLE: &str = "unreadable";
    /// The target rejected the record; the detail is its SPEC §12.2 error.
    pub const TARGET_REJECTED: &str = "target_rejected";
    /// A setting with no bee equivalent.
    pub const UNTRANSLATABLE: &str = "untranslatable";
    /// An LFS object GitHub did not serve.
    pub const LFS_FETCH_FAILED: &str = "lfs_fetch_failed";
    /// Downloaded bytes did not match their digest.
    pub const INTEGRITY: &str = "integrity";
    /// A release asset GitHub did not serve.
    pub const ASSET_FETCH_FAILED: &str = "asset_fetch_failed";
    /// The record refers to a parent record that was dropped.
    pub const PARENT_DROPPED: &str = "parent_dropped";
}

/// What happened to one record.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    /// Carried with full fidelity.
    Imported,
    /// Kept as a read-only record or in a degraded form.
    Archived {
        /// How it was kept.
        note: String,
    },
    /// Left out on purpose (an option, a pending review); not a loss.
    Excluded {
        /// Why.
        note: String,
    },
    /// Not carried.
    Dropped {
        /// A stable reason ([`reason`]).
        reason: String,
        /// Human-readable detail.
        detail: String,
    },
}

/// One record's outcome.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Outcome {
    /// Report category ([`category`]).
    pub category: String,
    /// The record (`pull/12`, `pull/12#review-comment-99`, `issue/7`, `label:bug`, ...).
    pub item: String,
    /// What happened.
    #[serde(flatten)]
    pub status: Status,
}

impl Outcome {
    /// Imported.
    pub fn imported(category: &str, item: impl Into<String>) -> Outcome {
        Outcome {
            category: category.into(),
            item: item.into(),
            status: Status::Imported,
        }
    }

    /// Archived with a note.
    pub fn archived(category: &str, item: impl Into<String>, note: impl Into<String>) -> Outcome {
        Outcome {
            category: category.into(),
            item: item.into(),
            status: Status::Archived { note: note.into() },
        }
    }

    /// Excluded with a note.
    pub fn excluded(category: &str, item: impl Into<String>, note: impl Into<String>) -> Outcome {
        Outcome {
            category: category.into(),
            item: item.into(),
            status: Status::Excluded { note: note.into() },
        }
    }

    /// Dropped with a reason.
    pub fn dropped(
        category: &str,
        item: impl Into<String>,
        reason: &str,
        detail: impl Into<String>,
    ) -> Outcome {
        Outcome {
            category: category.into(),
            item: item.into(),
            status: Status::Dropped {
                reason: reason.into(),
                detail: detail.into(),
            },
        }
    }

    /// True when dropped.
    pub fn is_dropped(&self) -> bool {
        matches!(self.status, Status::Dropped { .. })
    }
}

/// A placeholder principal in the report (claim tokens are secrets and never appear).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceholderRecord {
    /// GitHub numeric user id.
    pub github_id: u64,
    /// GitHub login.
    pub login: String,
    /// The principal id.
    pub principal: String,
    /// `human` or `agent`.
    pub kind: String,
    /// Already claimed by a real principal.
    pub claimed: bool,
    /// Records attributed to it (PRs, reviews, comments, issues, reactions, releases).
    pub contributions: u64,
}

/// A setting with no bee equivalent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Untranslatable {
    /// The setting (`branch_protection.required_conversation_resolution`, ...).
    pub setting: String,
    /// Where (branch pattern, CODEOWNERS line).
    pub item: String,
    /// What a reviewer should do.
    pub detail: String,
}

/// The `github` section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSection {
    /// `owner/repo`.
    pub repository: String,
    /// Phase reached (`mirror`, `live`, `frozen`, `cut_over`, `dry_run`).
    pub phase: String,
    /// Where metadata came from: `git`, `api`, `migration_archive`.
    pub sources: Vec<String>,
    /// Placeholder principals, by GitHub id.
    pub placeholders: Vec<PlaceholderRecord>,
    /// Things a person must do (re-enter secrets, enable webhooks, review land rules).
    pub manual_steps: Vec<String>,
    /// Settings that were not translated.
    pub untranslatable: Vec<Untranslatable>,
    /// Entries in the redirect map.
    pub redirects: u64,
    /// Dropped items without a reason (always zero; kept so readers can check).
    pub unexplained_drops: u64,
}

/// The full report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FidelityReport {
    /// The git report with the GitHub categories appended; `complete` and `summary` cover
    /// both.
    pub git: ImportReport,
    /// The GitHub section.
    pub github: GithubSection,
}

impl FidelityReport {
    /// Builds the report from the git report, every outcome and the section.
    pub fn build(
        mut git: ImportReport,
        outcomes: &[Outcome],
        mut github: GithubSection,
    ) -> FidelityReport {
        let base: Vec<&str> = bee_import::report::category::ALL.to_vec();
        git.categories
            .retain(|c| base.contains(&c.category.as_str()));
        let mut cats: Vec<Category> = category::ALL
            .iter()
            .map(|c| Category {
                category: (*c).to_string(),
                ..Category::default()
            })
            .collect();
        let mut unexplained = 0;
        for o in outcomes {
            let idx = match cats.iter().position(|c| c.category == o.category) {
                Some(i) => i,
                None => {
                    cats.push(Category {
                        category: o.category.clone(),
                        ..Category::default()
                    });
                    cats.len() - 1
                }
            };
            let c = &mut cats[idx];
            match &o.status {
                Status::Imported => c.imported += 1,
                Status::Archived { .. } => c.archived += 1,
                Status::Excluded { .. } => c.excluded += 1,
                Status::Dropped { reason, detail } => {
                    if reason.is_empty() {
                        unexplained += 1;
                    }
                    c.dropped += 1;
                    c.drops.push(DroppedItem {
                        item: o.item.clone(),
                        reason: reason.clone(),
                        detail: detail.clone(),
                    });
                }
            }
        }
        for c in &mut cats {
            c.drops.sort();
        }
        git.categories.extend(cats);
        github.unexplained_drops = unexplained;
        github.placeholders.sort_by_key(|p| p.github_id);
        github.untranslatable.sort();
        github.manual_steps.sort();
        github.manual_steps.dedup();
        github.sources.sort();
        github.sources.dedup();
        let dropped: u64 = git.categories.iter().map(|c| c.dropped).sum();
        let verified = !git.verification.performed || git.verification.passed();
        git.complete = dropped == 0 && verified && unexplained == 0;
        git.summary = if git.complete {
            "imported 100%".to_string()
        } else {
            let mut parts: Vec<String> = git
                .categories
                .iter()
                .filter(|c| c.dropped > 0)
                .map(|c| format!("{} {} dropped", c.dropped, c.category))
                .collect();
            if !verified {
                parts.push("round-trip verification failed".into());
            }
            format!("incomplete: {}", parts.join(", "))
        };
        FidelityReport { git, github }
    }

    /// True only when nothing was dropped and verification passed.
    pub fn complete(&self) -> bool {
        self.git.complete
    }

    /// A category.
    pub fn category(&self, name: &str) -> Option<&Category> {
        self.git.category(name)
    }

    /// The report as a JSON value (`schemas/v1/import-report.schema.json`).
    pub fn to_value(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(&self.git).unwrap_or_default();
        if let Some(m) = v.as_object_mut() {
            m.insert(
                "github".into(),
                serde_json::to_value(&self.github).unwrap_or_default(),
            );
        }
        v
    }

    /// The report as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.to_value()).unwrap_or_default()
    }

    /// Parses a report written by [`FidelityReport::to_json`].
    pub fn from_json(text: &str) -> serde_json::Result<FidelityReport> {
        let mut v: serde_json::Value = serde_json::from_str(text)?;
        let github = v
            .as_object_mut()
            .and_then(|m| m.remove("github"))
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        Ok(FidelityReport {
            git: serde_json::from_value(v)?,
            github,
        })
    }

    /// A short human summary: the summary line and one line per category with activity.
    pub fn human(&self) -> String {
        let mut out = format!("{}: {}\n", self.git.source, self.git.summary);
        for c in &self.git.categories {
            if c.imported + c.archived + c.dropped + c.excluded == 0 {
                continue;
            }
            out.push_str(&format!(
                "  {:<20} imported {:>6}  archived {:>5}  excluded {:>5}  dropped {:>5}\n",
                c.category, c.imported, c.archived, c.excluded, c.dropped
            ));
            for d in c.drops.iter().take(5) {
                out.push_str(&format!(
                    "      - {} ({}): {}\n",
                    d.item, d.reason, d.detail
                ));
            }
            if c.drops.len() > 5 {
                out.push_str(&format!("      ... {} more\n", c.drops.len() - 5));
            }
        }
        for s in &self.github.manual_steps {
            out.push_str(&format!("  todo: {s}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_import::report::ReportOptions;

    #[test]
    fn drops_make_the_report_incomplete_and_are_listed() {
        let git = ImportReport::new("github:o/r", "sha1", ReportOptions::default());
        let outcomes = vec![
            Outcome::imported(category::PULL_REQUESTS, "pull/1"),
            Outcome::dropped(
                category::FORCE_PUSH_COMMITS,
                "pull/1@abc",
                reason::FORCE_PUSH_COMMIT_UNREACHABLE,
                "gone",
            ),
            Outcome::archived(category::REVIEW_COMMENTS, "c1", "anchor moved"),
        ];
        let r = FidelityReport::build(git, &outcomes, GithubSection::default());
        assert!(!r.complete());
        assert_eq!(r.git.summary, "incomplete: 1 force_push_commits dropped");
        assert_eq!(r.category(category::PULL_REQUESTS).unwrap().imported, 1);
        assert_eq!(r.category(category::REVIEW_COMMENTS).unwrap().archived, 1);
        let back = FidelityReport::from_json(&r.to_json()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn nothing_dropped_is_complete() {
        let git = ImportReport::new("github:o/r", "sha1", ReportOptions::default());
        let r = FidelityReport::build(
            git,
            &[Outcome::imported(category::ISSUES, "issue/1")],
            GithubSection::default(),
        );
        assert!(r.complete());
        assert_eq!(r.git.summary, "imported 100%");
    }
}
