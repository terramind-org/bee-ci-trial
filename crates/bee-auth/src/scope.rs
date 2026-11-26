//! Token scopes: what one block allows, intersection across blocks, and request checks.

use std::collections::BTreeSet;

use bee_policy::{
    Action, PathMode, RefPattern, intersect_patterns, normalize_prefix, prefix_covers,
    reduce_patterns,
};
use serde::{Deserialize, Serialize};

use crate::error::{AuthError, AuthReason};

/// Limits on what a token chain may consume. `None` means unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Changesets created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changesets: Option<u64>,
    /// Changes landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_landed: Option<u64>,
    /// Bytes uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upload_bytes: Option<u64>,
}

impl Budget {
    /// Field-wise minimum.
    pub fn min(&self, other: &Budget) -> Budget {
        Budget {
            max_changesets: min_opt(self.max_changesets, other.max_changesets),
            max_landed: min_opt(self.max_landed, other.max_landed),
            max_upload_bytes: min_opt(self.max_upload_bytes, other.max_upload_bytes),
        }
    }

    fn within(&self, outer: &Budget) -> bool {
        let le = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => a <= b,
        };
        le(self.max_changesets, outer.max_changesets)
            && le(self.max_landed, outer.max_landed)
            && le(self.max_upload_bytes, outer.max_upload_bytes)
    }
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// What one token block allows. Every dimension is optional: `None` inherits (places no
/// restriction), `Some(empty set)` allows nothing in that dimension.
///
/// Repos and refs are patterns (exact, or a prefix with one trailing `*`). Paths are
/// component-wise prefixes; `/` is the whole repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Repo ids or patterns such as `org/*`.
    pub repos: Option<BTreeSet<RefPattern>>,
    /// Readable path prefixes (normalized, `""` is the root).
    pub read_paths: Option<BTreeSet<String>>,
    /// Writable path prefixes.
    pub write_paths: Option<BTreeSet<String>>,
    /// Ref patterns.
    pub refs: Option<BTreeSet<RefPattern>>,
    /// Allowed actions.
    pub actions: Option<BTreeSet<Action>>,
    /// Expiry, unix seconds. The token is valid while `now < expires_at`.
    pub expires_at: Option<u64>,
    /// Budget for this block (charged for every request made with this token or any token
    /// derived from it).
    pub budget: Option<Budget>,
    /// Opaque agent session id copied into provenance; the innermost block that sets one wins.
    pub session: Option<String>,
}

fn invalid(m: String) -> AuthError {
    AuthError::new(AuthReason::InvalidArgument, m)
}

fn patterns<I, S>(items: I) -> Result<BTreeSet<RefPattern>, AuthError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut v: Vec<RefPattern> = items
        .into_iter()
        .map(|s| RefPattern::parse(s.as_ref()).map_err(invalid))
        .collect::<Result<_, _>>()?;
    reduce_patterns(&mut v);
    Ok(v.into_iter().collect())
}

fn prefixes<I, S>(items: I) -> Result<BTreeSet<String>, AuthError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let v: Vec<String> = items
        .into_iter()
        .map(|s| normalize_prefix(s.as_ref()).map_err(invalid))
        .collect::<Result<_, _>>()?;
    Ok(reduce_prefixes(v))
}

/// Sorts, deduplicates and drops prefixes covered by another prefix in the set.
fn reduce_prefixes(mut v: Vec<String>) -> BTreeSet<String> {
    v.sort();
    v.dedup();
    let snapshot = v.clone();
    v.retain(|p| !snapshot.iter().any(|q| q != p && prefix_covers(q, p)));
    v.into_iter().collect()
}

fn intersect_prefixes(a: &BTreeSet<String>, b: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            if prefix_covers(x, y) {
                out.push(y.clone());
            } else if prefix_covers(y, x) {
                out.push(x.clone());
            }
        }
    }
    reduce_prefixes(out)
}

fn meet<T: Clone>(a: &Option<T>, b: &Option<T>, f: impl Fn(&T, &T) -> T) -> Option<T> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) => Some(f(x, y)),
    }
}

impl Scope {
    /// An unrestricted scope (every dimension inherits).
    pub fn new() -> Scope {
        Scope::default()
    }

    /// Restricts repos.
    pub fn repos<I: IntoIterator<Item = S>, S: AsRef<str>>(
        mut self,
        items: I,
    ) -> Result<Scope, AuthError> {
        self.repos = Some(patterns(items)?);
        Ok(self)
    }

    /// Restricts readable paths.
    pub fn read_paths<I: IntoIterator<Item = S>, S: AsRef<str>>(
        mut self,
        items: I,
    ) -> Result<Scope, AuthError> {
        self.read_paths = Some(prefixes(items)?);
        Ok(self)
    }

    /// Restricts writable paths.
    pub fn write_paths<I: IntoIterator<Item = S>, S: AsRef<str>>(
        mut self,
        items: I,
    ) -> Result<Scope, AuthError> {
        self.write_paths = Some(prefixes(items)?);
        Ok(self)
    }

    /// Restricts refs.
    pub fn refs<I: IntoIterator<Item = S>, S: AsRef<str>>(
        mut self,
        items: I,
    ) -> Result<Scope, AuthError> {
        self.refs = Some(patterns(items)?);
        Ok(self)
    }

    /// Restricts actions.
    pub fn actions<I: IntoIterator<Item = Action>>(mut self, items: I) -> Scope {
        self.actions = Some(items.into_iter().collect());
        self
    }

    /// Sets the expiry (unix seconds).
    pub fn expires_at(mut self, at: u64) -> Scope {
        self.expires_at = Some(at);
        self
    }

    /// Sets the budget.
    pub fn budget(mut self, budget: Budget) -> Scope {
        self.budget = Some(budget);
        self
    }

    /// Sets the session id.
    pub fn session(mut self, session: impl Into<String>) -> Scope {
        self.session = Some(session.into());
        self
    }

    /// The intersection of two scopes: a request is allowed by the result exactly when both
    /// allow it. Expiry is the earlier one, budgets the field-wise minimum, and the session is
    /// `inner`'s when set.
    pub fn intersect(&self, inner: &Scope) -> Scope {
        Scope {
            repos: meet(&self.repos, &inner.repos, |a, b| {
                let v: Vec<RefPattern> = a.iter().cloned().collect();
                let w: Vec<RefPattern> = b.iter().cloned().collect();
                intersect_patterns(&v, &w).into_iter().collect()
            }),
            read_paths: meet(&self.read_paths, &inner.read_paths, intersect_prefixes),
            write_paths: meet(&self.write_paths, &inner.write_paths, intersect_prefixes),
            refs: meet(&self.refs, &inner.refs, |a, b| {
                let v: Vec<RefPattern> = a.iter().cloned().collect();
                let w: Vec<RefPattern> = b.iter().cloned().collect();
                intersect_patterns(&v, &w).into_iter().collect()
            }),
            actions: meet(&self.actions, &inner.actions, |a, b| {
                a.intersection(b).copied().collect()
            }),
            expires_at: min_opt(self.expires_at, inner.expires_at),
            budget: meet(&self.budget, &inner.budget, Budget::min),
            session: inner.session.clone().or_else(|| self.session.clone()),
        }
    }

    /// Whether this scope allows nothing more than `outer` does.
    pub fn is_within(&self, outer: &Scope) -> bool {
        fn dim<T: Ord>(
            inner: &Option<BTreeSet<T>>,
            outer: &Option<BTreeSet<T>>,
            covers: impl Fn(&T, &T) -> bool,
        ) -> bool {
            match (inner, outer) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(i), Some(o)) => i.iter().all(|x| o.iter().any(|y| covers(y, x))),
            }
        }
        dim(&self.repos, &outer.repos, RefPattern::covers)
            && dim(&self.read_paths, &outer.read_paths, |o, i| {
                prefix_covers(o, i)
            })
            && dim(&self.write_paths, &outer.write_paths, |o, i| {
                prefix_covers(o, i)
            })
            && dim(&self.refs, &outer.refs, RefPattern::covers)
            && dim(&self.actions, &outer.actions, |o, i| o == i)
            && match (self.expires_at, outer.expires_at) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(i), Some(o)) => i <= o,
            }
            && match (&self.budget, &outer.budget) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(i), Some(o)) => i.within(o),
            }
    }

    /// Whether `repos` lists the repo id `id` as an exact entry (not through a pattern). Only
    /// then may a request name a repo by its id; see [`crate::VerifiedToken::authorize_repo`].
    pub fn lists_repo_id(&self, id: &str) -> bool {
        !id.is_empty()
            && self
                .repos
                .as_ref()
                .is_some_and(|r| r.contains(&RefPattern::Exact(id.to_string())))
    }

    /// Checks a request against this scope (expiry is checked at verification).
    pub fn check(&self, req: &Request) -> Result<(), AuthError> {
        let deny = |m: String| Err(AuthError::new(AuthReason::PolicyDenied, m));
        if let Some(repos) = &self.repos
            && !repos.iter().any(|p| p.matches(&req.repo))
        {
            return deny(format!("token does not cover repo {}", req.repo));
        }
        if let Some(actions) = &self.actions
            && !actions.contains(&req.action)
        {
            return deny(format!("token does not allow action `{}`", req.action));
        }
        let paths = match req.action.path_mode() {
            PathMode::Read => self.read_paths.as_ref().map(|p| (p, "read")),
            PathMode::Write => self.write_paths.as_ref().map(|p| (p, "write")),
            PathMode::None => None,
        };
        if let Some((allowed, mode)) = paths {
            for path in &req.paths {
                let path = request_path(path);
                if !allowed.iter().any(|p| prefix_covers(p, path)) {
                    return deny(format!("token does not allow {mode} on /{path}"));
                }
            }
        }
        if let (Some(refs), Some(name)) = (&self.refs, &req.ref_name)
            && !refs.iter().any(|p| p.matches(name))
        {
            return deny(format!("token does not cover ref {name}"));
        }
        Ok(())
    }

    /// Whether this scope's path prefixes for `action`'s mode cover `path` (always true when
    /// it sets none). [`Scope::check`] of a request naming only `path` passes exactly when the
    /// same request without paths passes and this is true.
    pub fn allows_path(&self, action: Action, path: &str) -> bool {
        let allowed = match action.path_mode() {
            PathMode::Read => self.read_paths.as_ref(),
            PathMode::Write => self.write_paths.as_ref(),
            PathMode::None => None,
        };
        allowed.is_none_or(|a| {
            let path = request_path(path);
            a.iter().any(|p| prefix_covers(p, path))
        })
    }

    pub(crate) fn to_wire(&self) -> ScopeWire {
        ScopeWire {
            repos: self
                .repos
                .as_ref()
                .map(|s| s.iter().map(ToString::to_string).collect()),
            read_paths: self
                .read_paths
                .as_ref()
                .map(|s| s.iter().map(|p| format!("/{p}")).collect()),
            write_paths: self
                .write_paths
                .as_ref()
                .map(|s| s.iter().map(|p| format!("/{p}")).collect()),
            refs: self
                .refs
                .as_ref()
                .map(|s| s.iter().map(ToString::to_string).collect()),
            actions: self
                .actions
                .as_ref()
                .map(|s| s.iter().map(|a| a.as_str().to_string()).collect()),
            expires_at: self.expires_at,
            budget: self.budget,
            session: self.session.clone(),
        }
    }

    pub(crate) fn from_wire(w: &ScopeWire) -> Result<Scope, AuthError> {
        Ok(Scope {
            repos: w.repos.as_ref().map(patterns).transpose()?,
            read_paths: w.read_paths.as_ref().map(prefixes).transpose()?,
            write_paths: w.write_paths.as_ref().map(prefixes).transpose()?,
            refs: w.refs.as_ref().map(patterns).transpose()?,
            actions: w
                .actions
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|a| {
                            Action::parse(a).ok_or_else(|| invalid(format!("unknown action `{a}`")))
                        })
                        .collect::<Result<BTreeSet<_>, _>>()
                })
                .transpose()?,
            expires_at: w.expires_at,
            budget: w.budget,
            session: w.session.clone(),
        })
    }
}

/// Normalizes a request path (no leading or trailing `/`).
pub(crate) fn request_path(p: &str) -> &str {
    p.trim_start_matches('/').trim_end_matches('/')
}

/// The JSON form of a scope carried in a block's context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// One request to authorize against a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Repo id.
    pub repo: String,
    /// Action.
    pub action: Action,
    /// Paths touched (read for read actions, written for write actions).
    pub paths: Vec<String>,
    /// Ref involved, if any.
    pub ref_name: Option<String>,
}

impl Request {
    /// A request with no paths and no ref.
    pub fn new(repo: impl Into<String>, action: Action) -> Request {
        Request {
            repo: repo.into(),
            action,
            paths: Vec::new(),
            ref_name: None,
        }
    }

    /// Adds paths.
    pub fn paths<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> Request {
        self.paths.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Sets the ref.
    pub fn on_ref(mut self, name: impl Into<String>) -> Request {
        self.ref_name = Some(name.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_narrows_each_dimension() {
        let outer = Scope::new()
            .repos(["org/*"])
            .unwrap()
            .write_paths(["/"])
            .unwrap()
            .actions([Action::Read, Action::Commit, Action::Land])
            .expires_at(100)
            .budget(Budget {
                max_changesets: Some(10),
                ..Budget::default()
            });
        let inner = Scope::new()
            .repos(["org/a", "other/b"])
            .unwrap()
            .write_paths(["docs", "docs/api"])
            .unwrap()
            .actions([Action::Commit, Action::Review])
            .expires_at(200)
            .budget(Budget {
                max_changesets: Some(5),
                max_landed: Some(1),
                ..Budget::default()
            })
            .session("s1");
        let e = outer.intersect(&inner);
        assert_eq!(
            e.repos
                .unwrap()
                .into_iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
            ["org/a"]
        );
        assert_eq!(
            e.write_paths.unwrap().into_iter().collect::<Vec<_>>(),
            ["docs"]
        );
        assert_eq!(
            e.actions.unwrap().into_iter().collect::<Vec<_>>(),
            [Action::Commit]
        );
        assert_eq!(e.expires_at, Some(100));
        assert_eq!(e.budget.unwrap().max_changesets, Some(5));
        assert_eq!(e.budget.unwrap().max_landed, Some(1));
        assert_eq!(e.session.as_deref(), Some("s1"));
    }

    #[test]
    fn allows_path_splits_check() {
        let scopes = [
            Scope::new(),
            Scope::new().read_paths(["docs", "src/a"]).unwrap(),
            Scope::new().write_paths(["docs"]).unwrap(),
            Scope::new()
                .read_paths(["docs"])
                .unwrap()
                .write_paths(["src"])
                .unwrap(),
        ];
        let paths = [
            "",
            "/",
            "docs",
            "docs/a.md",
            "/docs/a.md",
            "docs2/x",
            "src/a/b",
            "src/b",
        ];
        for s in &scopes {
            for action in Action::ALL {
                let base = s.check(&Request::new("r", action)).is_ok();
                for path in paths {
                    let one = s.check(&Request::new("r", action).paths([path])).is_ok();
                    assert_eq!(
                        one,
                        base && s.allows_path(action, path),
                        "{action} {path:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn checks_requests() {
        let s = Scope::new()
            .write_paths(["docs"])
            .unwrap()
            .refs(["ws/*"])
            .unwrap();
        let ok = Request::new("r", Action::Commit)
            .paths(["docs/a.md"])
            .on_ref("ws/a/x");
        assert!(s.check(&ok).is_ok());
        let out = Request::new("r", Action::Commit).paths(["docs2/a.md"]);
        assert_eq!(s.check(&out).unwrap_err().reason, AuthReason::PolicyDenied);
        let bad_ref = Request::new("r", Action::Commit)
            .paths(["docs/a"])
            .on_ref("main");
        assert!(s.check(&bad_ref).is_err());
        // Reads are not limited by write paths.
        assert!(
            s.check(&Request::new("r", Action::Read).paths(["src/x"]))
                .is_ok()
        );
    }

    #[test]
    fn wire_round_trip() {
        let s = Scope::new()
            .repos(["a", "org/*"])
            .unwrap()
            .read_paths(["/"])
            .unwrap()
            .write_paths(Vec::<String>::new())
            .unwrap()
            .actions([Action::Read])
            .session("x");
        assert_eq!(Scope::from_wire(&s.to_wire()).unwrap(), s);
    }
}
