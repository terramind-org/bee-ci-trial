//! The Git LFS file-locking API (`<repo>.git/info/lfs/locks`).
//!
//! | Route | Use |
//! |---|---|
//! | `POST locks` | lock a path (`201`), or `409` with the lock that holds it |
//! | `GET locks` | list locks; filters `path`, `id`; paging `cursor`, `limit` |
//! | `POST locks/verify` | the caller's locks (`ours`) and everyone else's (`theirs`), for `lfs.locksverify` |
//! | `POST locks/<id>/unlock` | release a lock; `force` breaks someone else's |
//!
//! Locks belong to the repo (the optional `ref` of the protocol is accepted and ignored: a
//! binary file is locked for every branch). Taking a lock needs write access to the path, like a
//! commit that changes it; releasing needs ownership, or `force` and the `force_update`
//! permission. A lock is stored by the server through [`LockStore`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::backend::RepoSession;
use crate::error::{GitError, GitResult};

/// The most locks one page carries.
pub const MAX_PAGE: usize = 100;

/// One lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    /// The lock id.
    pub id: String,
    /// The locked repo-relative path, `/`-separated.
    pub path: String,
    /// When the lock was taken, milliseconds since the Unix epoch.
    pub locked_at_ms: u64,
    /// The owning principal's id.
    pub owner: String,
    /// The owner's name as clients show it.
    pub owner_name: String,
}

/// Where locks are kept (the server implements it over its own tables).
#[async_trait]
pub trait LockStore: Send + Sync {
    /// Stores a new lock if no lock holds its path: `None` when created, `Some(existing)` when
    /// the path is taken. Atomic.
    async fn create(&self, repo: &str, lock: &Lock) -> GitResult<Option<Lock>>;

    /// A lock by id.
    async fn by_id(&self, repo: &str, id: &str) -> GitResult<Option<Lock>>;

    /// The lock on a path.
    async fn by_path(&self, repo: &str, path: &str) -> GitResult<Option<Lock>>;

    /// Locks ordered by path, with a path strictly after `after`, at most `limit`.
    async fn list(&self, repo: &str, after: Option<&str>, limit: usize) -> GitResult<Vec<Lock>>;

    /// Removes a lock; false when it was already gone. Atomic with respect to `create`.
    async fn remove(&self, repo: &str, lock: &Lock) -> GitResult<bool>;
}

/// A lock as the protocol shows it.
pub fn lock_json(l: &Lock) -> Value {
    json!({
        "id": l.id,
        "path": l.path,
        "locked_at": rfc3339(l.locked_at_ms),
        "owner": { "name": l.owner_name },
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time in milliseconds.
pub fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Checks a lock path: relative, `/`-separated, no empty, `.` or `..` segment.
pub fn check_path(path: &str) -> GitResult<()> {
    let bad = |why: &str| GitError::invalid(format!("bad lock path {path:?}: {why}"));
    if path.is_empty() || path.len() > 4096 {
        return Err(bad("empty or too long"));
    }
    if path.contains('\0') || path.contains('\\') {
        return Err(bad("control or backslash character"));
    }
    if path
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(bad("must be a normalized relative path"));
    }
    Ok(())
}

fn new_id(repo: &str, path: &str, principal: &str, at_ms: u64) -> String {
    let mut h = blake3::Hasher::new();
    for part in [repo, path, principal] {
        h.update(part.as_bytes());
        h.update(&[0]);
    }
    h.update(&at_ms.to_le_bytes());
    bee_core::hex::encode(&h.finalize().as_bytes()[..16])
}

fn page_size(limit: Option<usize>) -> usize {
    limit
        .filter(|l| *l > 0)
        .map_or(MAX_PAGE, |l| l.min(MAX_PAGE))
}

#[derive(Deserialize)]
struct CreateBody {
    path: String,
}

/// `POST locks`: `(status, body)`.
pub async fn create(
    session: &dyn RepoSession,
    store: &dyn LockStore,
    body: &[u8],
    now_ms: u64,
) -> GitResult<(u16, Value)> {
    let req: CreateBody = serde_json::from_slice(body)
        .map_err(|e| GitError::invalid(format!("bad lock request: {e}")))?;
    check_path(&req.path)?;
    if !session.path_writable(&req.path) {
        return Err(GitError::named(
            "PERMISSION_DENIED",
            format!("you may not write {}, so you may not lock it", req.path),
        ));
    }
    let lock = Lock {
        id: new_id(session.full_name(), &req.path, session.principal(), now_ms),
        path: req.path,
        locked_at_ms: now_ms,
        owner: session.principal().to_string(),
        owner_name: session.principal_label(),
    };
    match store.create(session.full_name(), &lock).await? {
        None => Ok((201, json!({ "lock": lock_json(&lock) }))),
        Some(held) => Ok((
            409,
            json!({ "lock": lock_json(&held), "message": "already created lock" }),
        )),
    }
}

/// Query parameters of `GET locks`.
#[derive(Default)]
pub struct ListQuery {
    /// Only the lock on this path.
    pub path: Option<String>,
    /// Only the lock with this id.
    pub id: Option<String>,
    /// Resume after this cursor.
    pub cursor: Option<String>,
    /// Page size.
    pub limit: Option<usize>,
}

async fn page(
    store: &dyn LockStore,
    repo: &str,
    cursor: Option<&str>,
    limit: usize,
) -> GitResult<(Vec<Lock>, String)> {
    let after = cursor.filter(|c| !c.is_empty());
    // One extra row tells whether another page follows; the cursor is the last path returned.
    let mut rows = store.list(repo, after, limit + 1).await?;
    if rows.len() <= limit {
        return Ok((rows, String::new()));
    }
    rows.truncate(limit);
    let next = rows.last().map(|l| l.path.clone()).unwrap_or_default();
    Ok((rows, next))
}

/// `GET locks`.
pub async fn list(
    session: &dyn RepoSession,
    store: &dyn LockStore,
    q: &ListQuery,
) -> GitResult<Value> {
    let repo = session.full_name();
    if let Some(path) = &q.path {
        let l = store.by_path(repo, path).await?;
        let locks: Vec<Value> = l.iter().map(lock_json).collect();
        return Ok(json!({ "locks": locks, "next_cursor": "" }));
    }
    if let Some(id) = &q.id {
        let l = store.by_id(repo, id).await?;
        let locks: Vec<Value> = l.iter().map(lock_json).collect();
        return Ok(json!({ "locks": locks, "next_cursor": "" }));
    }
    let (locks, next) = page(store, repo, q.cursor.as_deref(), page_size(q.limit)).await?;
    let locks: Vec<Value> = locks.iter().map(lock_json).collect();
    Ok(json!({ "locks": locks, "next_cursor": next }))
}

#[derive(Deserialize, Default)]
struct VerifyBody {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `POST locks/verify`: the caller's locks and everyone else's. An empty body is accepted.
pub async fn verify(
    session: &dyn RepoSession,
    store: &dyn LockStore,
    body: &[u8],
) -> GitResult<Value> {
    let req: VerifyBody = if body.iter().all(u8::is_ascii_whitespace) {
        VerifyBody::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| GitError::invalid(format!("bad lock verify request: {e}")))?
    };
    let me = session.principal();
    let (locks, next) = page(
        store,
        session.full_name(),
        req.cursor.as_deref(),
        page_size(req.limit),
    )
    .await?;
    let (ours, theirs): (Vec<&Lock>, Vec<&Lock>) = locks.iter().partition(|l| l.owner == me);
    Ok(json!({
        "ours": ours.into_iter().map(lock_json).collect::<Vec<_>>(),
        "theirs": theirs.into_iter().map(lock_json).collect::<Vec<_>>(),
        "next_cursor": next,
    }))
}

#[derive(Deserialize, Default)]
struct UnlockBody {
    #[serde(default)]
    force: bool,
}

/// `POST locks/<id>/unlock`: `(status, body)`.
pub async fn unlock(
    session: &dyn RepoSession,
    store: &dyn LockStore,
    id: &str,
    body: &[u8],
) -> GitResult<(u16, Value)> {
    let req: UnlockBody = if body.iter().all(u8::is_ascii_whitespace) {
        UnlockBody::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| GitError::invalid(format!("bad unlock request: {e}")))?
    };
    let repo = session.full_name();
    let Some(lock) = store.by_id(repo, id).await? else {
        return Err(GitError::not_found(format!("no lock {id}")));
    };
    if lock.owner != session.principal() {
        if !req.force {
            return Err(GitError::named(
                "PERMISSION_DENIED",
                format!(
                    "{} is locked by {}; only the owner can unlock it (force unlock needs the \
                     force_update permission)",
                    lock.path, lock.owner_name
                ),
            ));
        }
        if !session.may_force_unlock().await? {
            return Err(GitError::named(
                "PERMISSION_DENIED",
                "breaking someone else's lock needs the force_update permission",
            ));
        }
    }
    if !store.remove(repo, &lock).await? {
        return Err(GitError::not_found(format!("no lock {id}")));
    }
    Ok((200, json!({ "lock": lock_json(&lock) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_373_348_403), "2026-10-19T01:29:08Z");
        assert_eq!(rfc3339(4_102_444_799_000), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn paths() {
        assert!(check_path("a/b.bin").is_ok());
        for bad in ["", "/a", "a/", "a//b", "a/../b", "./a", "a\\b", "a\0b"] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = new_id("o/r", "f.bin", "u1", 5);
        assert_eq!(a, new_id("o/r", "f.bin", "u1", 5));
        assert_ne!(a, new_id("o/r", "f.bin", "u2", 5));
        assert_ne!(a, new_id("o/r", "g.bin", "u1", 5));
        assert_eq!(a.len(), 32);
    }
}
