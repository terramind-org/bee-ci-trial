//! Links keep working (SPEC §17.3): rewriting `github.com/<owner>/<repo>/…` links inside
//! imported text, and the cutover redirect map from old GitHub URLs to Beehive.
//!
//! PR and issue numbers are preserved and Beehive serves `/<org>/<repo>/pull/<n>` and
//! `/<org>/<repo>/issues/<n>`, so a link changes only its host: rewriting replaces the
//! `https://github.com/<owner>/<repo>` prefix with the Beehive repository URL and leaves the
//! rest. Bare `#123` references need no rewrite: the number names the same item in Beehive.
//! Links to other repositories are left alone (they may not be imported).

use serde::{Deserialize, Serialize};

use crate::model::Metadata;

/// Rewrites links to `web/owner/repo` (`https://github.com/acme/api`) into `hub/org/repo`.
/// Returns the text unchanged when nothing matches.
pub fn rewrite(text: &str, web_repo: &str, hub_repo: &str) -> String {
    let web_repo = web_repo.trim_end_matches('/');
    let hub_repo = hub_repo.trim_end_matches('/');
    if web_repo.is_empty() || !text.contains(web_repo) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(web_repo) {
        let after = &rest[i + web_repo.len()..];
        // Only whole repository names: `acme/api` must not match `acme/api-docs`.
        let boundary = after.chars().next().is_none_or(|c| {
            matches!(
                c,
                '/' | '#'
                    | '?'
                    | ')'
                    | ']'
                    | '>'
                    | '"'
                    | '\''
                    | ' '
                    | '\n'
                    | '\t'
                    | '.'
                    | ','
                    | ';'
                    | ':'
                    | '!'
            )
        });
        out.push_str(&rest[..i]);
        if boundary {
            out.push_str(hub_repo);
        } else {
            out.push_str(web_repo);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// One redirect.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Redirect {
    /// The GitHub URL.
    pub from: String,
    /// The Beehive URL.
    pub to: String,
    /// `pull`, `issue`, `release`, `repo`.
    pub kind: String,
}

/// The redirect map written at cutover: exact entries for every PR, issue and release, and
/// prefix rules for everything else under the repository (commits, trees, blobs, compare).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectMap {
    /// Exact URL redirects, sorted.
    pub entries: Vec<Redirect>,
    /// Prefix rules (`from` prefix → `to` prefix), applied when no exact entry matches.
    pub prefixes: Vec<Redirect>,
}

impl RedirectMap {
    /// The map for a repository's metadata.
    pub fn build(meta: &Metadata, web_repo: &str, hub_repo: &str) -> RedirectMap {
        let w = web_repo.trim_end_matches('/');
        let h = hub_repo.trim_end_matches('/');
        let mut entries = Vec::new();
        for n in meta.pulls.keys() {
            entries.push(Redirect {
                from: format!("{w}/pull/{n}"),
                to: format!("{h}/pull/{n}"),
                kind: "pull".into(),
            });
        }
        for n in meta.issues.keys() {
            entries.push(Redirect {
                from: format!("{w}/issues/{n}"),
                to: format!("{h}/issues/{n}"),
                kind: "issue".into(),
            });
        }
        for r in meta.releases.values() {
            entries.push(Redirect {
                from: format!("{w}/releases/tag/{}", r.tag_name),
                to: format!("{h}/releases/tag/{}", r.tag_name),
                kind: "release".into(),
            });
        }
        entries.sort();
        RedirectMap {
            entries,
            prefixes: vec![Redirect {
                from: format!("{w}/"),
                to: format!("{h}/"),
                kind: "repo".into(),
            }],
        }
    }

    /// Where a GitHub URL goes, if anywhere.
    pub fn resolve(&self, url: &str) -> Option<String> {
        let url = url.trim_end_matches('/');
        if let Some(e) = self.entries.iter().find(|e| e.from == url) {
            return Some(e.to.clone());
        }
        // `/pull/12/files` and similar sub-pages follow their item.
        for e in &self.entries {
            if let Some(rest) = url.strip_prefix(&e.from)
                && rest.starts_with('/')
            {
                return Some(format!("{}{rest}", e.to));
            }
        }
        self.prefixes.iter().find_map(|p| {
            url.strip_prefix(p.from.trim_end_matches('/'))
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .map(|rest| format!("{}{rest}", p.to.trim_end_matches('/')))
        })
    }

    /// Number of entries (exact plus prefix rules).
    pub fn len(&self) -> usize {
        self.entries.len() + self.prefixes.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_this_repository() {
        let t = "see https://github.com/acme/api/pull/3 and https://github.com/acme/api-docs/x \
                 and (https://github.com/acme/api).";
        let r = rewrite(t, "https://github.com/acme/api", "https://hub.dev/acme/api");
        assert_eq!(
            r,
            "see https://hub.dev/acme/api/pull/3 and https://github.com/acme/api-docs/x \
                 and (https://hub.dev/acme/api)."
        );
        assert_eq!(
            rewrite("#12 only", "https://github.com/a/b", "h"),
            "#12 only"
        );
    }

    #[test]
    fn redirect_map_resolves_items_subpages_and_prefixes() {
        let mut m = Metadata::default();
        m.pulls.insert(
            12,
            crate::model::PullRequest {
                number: 12,
                ..Default::default()
            },
        );
        m.issues.insert(
            7,
            crate::model::Issue {
                number: 7,
                ..Default::default()
            },
        );
        let map = RedirectMap::build(&m, "https://github.com/a/r", "https://hub/a/r");
        assert_eq!(
            map.resolve("https://github.com/a/r/pull/12").unwrap(),
            "https://hub/a/r/pull/12"
        );
        assert_eq!(
            map.resolve("https://github.com/a/r/pull/12/files").unwrap(),
            "https://hub/a/r/pull/12/files"
        );
        assert_eq!(
            map.resolve("https://github.com/a/r/issues/7").unwrap(),
            "https://hub/a/r/issues/7"
        );
        assert_eq!(
            map.resolve("https://github.com/a/r/commit/abc").unwrap(),
            "https://hub/a/r/commit/abc"
        );
        assert_eq!(map.resolve("https://github.com/a/other/pull/1"), None);
        assert_eq!(map.len(), 3);
    }
}
