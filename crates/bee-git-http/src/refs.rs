//! Ref names between git and bee, as one principal sees them.
//!
//! | bee | git |
//! |---|---|
//! | `ws/<me>/<rest>` (the caller's own workspace) | `refs/heads/ws/<rest>` |
//! | `ws/<other>/<rest>` | `refs/workspaces/<other>/<rest>` |
//! | `tags/<t>` | `refs/tags/<t>` |
//! | `pull/<rest>` | `refs/pull/<rest>` |
//! | `git/<x>` | `refs/<x>` |
//! | any other `<b>` | `refs/heads/<b>` |
//!
//! Every bee ref name is a valid git ref name (SPEC §5 is stricter than git), so the bee →
//! git direction never fails. The git → bee direction refuses branch names that would land in
//! a bee namespace (`refs/heads/tags/…`, `refs/heads/pull/…`, `refs/heads/git/…`) and names
//! the bee grammar rejects, with `INVALID_ARGUMENT`.

use bee_core::RefName;

use crate::error::{GitError, GitResult};

/// The git name of a bee ref for principal `me`.
pub fn git_name(bee: &RefName, me: &str) -> String {
    let s = bee.as_str();
    if let Some(rest) = s.strip_prefix("ws/") {
        return match rest.split_once('/') {
            Some((owner, tail)) if owner == me => format!("refs/heads/ws/{tail}"),
            _ => format!("refs/workspaces/{rest}"),
        };
    }
    if let Some(t) = s.strip_prefix("tags/") {
        return format!("refs/tags/{t}");
    }
    if let Some(p) = s.strip_prefix("pull/") {
        return format!("refs/pull/{p}");
    }
    if let Some(x) = s.strip_prefix("git/") {
        return format!("refs/{x}");
    }
    format!("refs/heads/{s}")
}

fn parse(name: String) -> GitResult<RefName> {
    RefName::new(name.clone()).map_err(|e| {
        GitError::invalid(format!(
            "{name:?} is not a valid bee ref name: {}",
            e.message
        ))
    })
}

/// The bee ref a git ref name pushes to, for principal `me`.
pub fn bee_name(git: &str, me: &str) -> GitResult<RefName> {
    let Some(rest) = git.strip_prefix("refs/") else {
        return Err(GitError::invalid(format!(
            "{git:?} is not a full ref name (refs/...)"
        )));
    };
    if let Some(b) = rest.strip_prefix("heads/") {
        if let Some(tail) = b.strip_prefix("ws/") {
            return parse(format!("ws/{me}/{tail}"));
        }
        for ns in ["tags/", "pull/", "git/", "ws"] {
            if b.starts_with(ns) && (ns.ends_with('/') || b == ns) {
                return Err(GitError::invalid(format!(
                    "branch {b:?} collides with the bee `{}` namespace; pick another name",
                    ns.trim_end_matches('/')
                )));
            }
        }
        return parse(b.to_string());
    }
    if let Some(w) = rest.strip_prefix("workspaces/") {
        return parse(format!("ws/{w}"));
    }
    if let Some(t) = rest.strip_prefix("tags/") {
        return parse(format!("tags/{t}"));
    }
    if let Some(p) = rest.strip_prefix("pull/") {
        return parse(format!("pull/{p}"));
    }
    parse(format!("git/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }

    #[test]
    fn bee_to_git() {
        assert_eq!(git_name(&r("main"), "u1"), "refs/heads/main");
        assert_eq!(git_name(&r("release/1.0"), "u1"), "refs/heads/release/1.0");
        assert_eq!(git_name(&r("ws/u1/fix"), "u1"), "refs/heads/ws/fix");
        assert_eq!(git_name(&r("ws/u2/fix"), "u1"), "refs/workspaces/u2/fix");
        assert_eq!(git_name(&r("tags/v1"), "u1"), "refs/tags/v1");
        assert_eq!(git_name(&r("pull/7/head"), "u1"), "refs/pull/7/head");
        assert_eq!(git_name(&r("git/notes/x"), "u1"), "refs/notes/x");
    }

    #[test]
    fn git_to_bee_round_trips() {
        for (bee, me) in [
            ("main", "u1"),
            ("feature/a", "u1"),
            ("ws/u1/fix/deep", "u1"),
            ("ws/u2/fix", "u1"),
            ("tags/v1.0", "u1"),
            ("pull/3/head", "u1"),
            ("git/notes/commits", "u1"),
        ] {
            let g = git_name(&r(bee), me);
            assert_eq!(bee_name(&g, me).unwrap(), r(bee), "{g}");
        }
    }

    #[test]
    fn git_to_bee_refusals() {
        for bad in [
            "main",
            "refs/heads/tags/x",
            "refs/heads/pull/1",
            "refs/heads/git/x",
            "refs/heads/ws",
            "refs/heads/a@b",
            "refs/heads/ws/",
        ] {
            assert_eq!(
                bee_name(bad, "u1").unwrap_err().reason,
                "INVALID_ARGUMENT",
                "{bad}"
            );
        }
    }
}
