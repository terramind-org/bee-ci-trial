//! Path leases (SPEC §11.5).
//!
//! A lease announces intent to change paths of one ref before the work starts.
//!
//! # Overlap rules
//!
//! Two leases overlap when they are on the same ref and one leased path is a component-wise
//! prefix of the other (`src` overlaps `src/a.rs`, not `src2`; the empty path is the whole
//! repo). For a new lease by holder H:
//!
//! | existing lease (other holder) | new advisory | new exclusive |
//! |---|---|---|
//! | advisory, overlapping | ok, returned in `overlapping` | ok, returned in `overlapping` |
//! | exclusive, overlapping | `LEASED` | `LEASED` |
//! | any, not overlapping | ok | ok |
//!
//! Leases held by H itself never conflict with H's new lease.
//!
//! # Expiry and release
//!
//! Leases expire after their TTL (default 2 hours, at most 24) and may be renewed by their
//! holder. An expired lease is invisible immediately; [`Leases::sweep_expired`] deletes it and
//! emits `lease.expired`. Leases tied to a ChangeId are released when a `cr.landed` or
//! `cr.closed` event for that change arrives ([`AutoRelease`]) or when the changes module
//! calls [`Leases::release_for_change`]. Every acquire, renew and release is an event and an
//! audit entry.
//!
//! # Land path
//!
//! [`Leases::land_check`] answers "may this principal land these paths?" with `LEASED` when
//! an exclusive lease of another principal overlaps. The VCS land path must not depend on
//! forge tables (SPEC §15.1), so callers use [`Leases::land_check_fail_open`], which treats an
//! unreachable lease store as "no leases".
//!
//! Acquisition is serialized per repository by an internal guard row held from before the live
//! lease scan until the transaction commits.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{ChangeId, Event, Lease, LeaseMode, SubjectKind, event};
use prost::Message;

use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventListener;
use crate::ids::{hex, ts, ts_ms};
use crate::module::{EventKind, ForgeModule};
use crate::store::{
    Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef, Tx,
};

/// Default time to live.
pub const DEFAULT_TTL_SECS: u32 = 2 * 3600;
/// Longest time to live.
pub const MAX_TTL_SECS: u32 = 24 * 3600;
/// Most paths in one lease.
pub const MAX_PATHS: usize = 1000;
/// Most live leases in one repo.
pub const MAX_LEASES_PER_REPO: usize = 10_000;

/// Leases, scoped by repo; `k1` holder, `k2` change id (hex), `n1` expiry (ms).
pub static LEASES: TableDef = TableDef {
    name: "forge_leases",
    surface: Surface::LEASES,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::ScopeK1, Index::ScopeK2, Index::N1],
};

/// Internal per-repository acquisition guards. They are deliberately separate from `LEASES`
/// so they cannot be observed or counted as user leases.
pub static LEASE_GUARDS: TableDef = TableDef {
    name: "forge_lease_guards",
    surface: Surface::LEASES,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

static M1: Migration = Migration {
    module: "leases",
    version: 1,
    name: "create_leases",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&LEASES)],
};

static M2: Migration = Migration {
    module: "leases",
    version: 2,
    name: "create_acquisition_guards",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&LEASE_GUARDS)],
};

/// What the leases module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "leases",
    surfaces: &[Surface::LEASES],
    migrations: &[&M1, &M2],
    event_kinds: &[
        EventKind::new("lease.acquired", "A lease was acquired"),
        EventKind::new("lease.renewed", "A lease's expiry moved"),
        EventKind::new("lease.released", "A lease was released"),
        EventKind::new("lease.expired", "A lease expired"),
    ],
    audit_prefixes: &["lease."],
};

/// Normalizes a leased path: UTF-8, no leading or trailing `/`, no empty, `.` or `..`
/// components. The empty string is the whole repo.
pub fn normalize_path(raw: &[u8]) -> Result<String> {
    let s = std::str::from_utf8(raw)
        .map_err(|_| ForgeError::invalid("invalid_path", "lease paths must be UTF-8"))?;
    let s = s.trim_matches('/');
    if s.split('/').any(|c| c == "." || c == "..") || s.contains("//") || s.contains('\0') {
        return Err(ForgeError::invalid(
            "invalid_path",
            format!("bad lease path {s:?}"),
        ));
    }
    Ok(s.to_string())
}

/// Whether `prefix` covers `path` component-wise.
pub fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

/// Whether two path sets overlap.
pub fn paths_overlap(a: &[String], b: &[String]) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| covers(x, y) || covers(y, x)))
}

fn lease_paths(l: &Lease) -> Vec<String> {
    l.paths
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

fn holder_id(l: &Lease) -> &str {
    l.holder.as_ref().map(|h| h.id.as_str()).unwrap_or("")
}

fn exclusive(l: &Lease) -> bool {
    l.mode == LeaseMode::Exclusive as i32
}

/// A lease request.
#[derive(Clone, Debug, Default)]
pub struct AcquireRequest {
    /// Repo id.
    pub repo_id: String,
    /// Org id (for events and audit).
    pub org_id: String,
    /// Ref (the service fills the default ref when empty).
    pub ref_name: String,
    /// Raw paths.
    pub paths: Vec<Vec<u8>>,
    /// Mode (unspecified = advisory).
    pub mode: LeaseMode,
    /// TTL in seconds (0 = default).
    pub ttl_seconds: u32,
    /// ChangeId the lease is for.
    pub change_id: Option<ChangeId>,
    /// Free text.
    pub note: String,
}

/// Leases over the store.
pub struct Leases {
    db: Db,
}

impl Leases {
    /// Leases over `db`.
    pub fn new(db: Db) -> Leases {
        Leases { db }
    }

    async fn live(tx: &mut Tx, repo_id: &str, now: u64) -> Result<Vec<(Lease, Row)>> {
        tx.scan_msgs::<Lease>(
            &LEASES,
            &Scan::scope(repo_id, MAX_LEASES_PER_REPO)
                .by_n1()
                .n1_min(now as i64 + 1),
        )
        .await
    }

    async fn emit(tx: &mut Tx, kind: &str, org_id: &str, lease: &Lease) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: lease.repo_id.clone(),
            org_id: org_id.into(),
            body: Some(event::Body::Lease(lease.clone())),
            ..Default::default()
        })
        .await
    }

    fn audit(action: &str, org_id: &str, lease: &Lease) -> AuditDraft {
        AuditDraft::new(action, org_id)
            .repo_id(&lease.repo_id)
            .target(SubjectKind::Unspecified, &lease.id)
            .after(&json(lease))
    }

    /// Acquires a lease; returns it and the overlapping advisory leases of others.
    pub async fn acquire(&self, cx: &Cx, req: AcquireRequest) -> Result<(Lease, Vec<Lease>)> {
        if req.paths.is_empty() {
            return Err(ForgeError::missing("paths"));
        }
        if req.paths.len() > MAX_PATHS {
            return Err(ForgeError::too_large(
                "lease_paths",
                MAX_PATHS as u64,
                req.paths.len() as u64,
            ));
        }
        let ttl = match req.ttl_seconds {
            0 => DEFAULT_TTL_SECS,
            t if t > MAX_TTL_SECS => {
                return Err(ForgeError::invalid(
                    "ttl_too_long",
                    format!("ttl_seconds is at most {MAX_TTL_SECS}"),
                ));
            }
            t => t,
        };
        let mut paths = req
            .paths
            .iter()
            .map(|p| normalize_path(p))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        let mode = if req.mode == LeaseMode::Exclusive {
            LeaseMode::Exclusive
        } else {
            LeaseMode::Advisory
        };
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        // The atomic upsert takes the Postgres row lock even when this is the first acquire for
        // a repository; it remains held until commit, serializing the live scan and insert.
        tx.add_n1(&LEASE_GUARDS, &req.repo_id, "acquire", 1).await?;
        let now = tx.now_ms();
        let live = Self::live(&mut tx, &req.repo_id, now).await?;
        if live.len() >= MAX_LEASES_PER_REPO {
            return Err(ForgeError::too_large(
                "leases_per_repo",
                MAX_LEASES_PER_REPO as u64,
                live.len() as u64 + 1,
            ));
        }
        let mut conflicts = Vec::new();
        let mut overlapping = Vec::new();
        for (l, _) in live {
            if l.r#ref != req.ref_name || holder_id(&l) == cx.actor_id {
                continue;
            }
            if !paths_overlap(&paths, &lease_paths(&l)) {
                continue;
            }
            if exclusive(&l) {
                conflicts.push(l);
            } else {
                overlapping.push(l);
            }
        }
        if !conflicts.is_empty() {
            let conflict_paths = conflicts
                .iter()
                .flat_map(|l| l.paths.iter().cloned())
                .collect();
            return Err(ForgeError::leased(
                conflicts.iter().map(|l| l.id.clone()).collect(),
                conflict_paths,
            ));
        }
        let id = tx.new_id()?;
        let expires = now + u64::from(ttl) * 1000;
        let lease = Lease {
            id: id.clone(),
            repo_id: req.repo_id.clone(),
            r#ref: req.ref_name.clone(),
            paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
            holder: Some(cx.principal()),
            change_id: req.change_id.clone(),
            mode: mode as i32,
            expires_at: Some(ts(expires)),
            note: req.note.clone(),
        };
        let mut row = Row::new(req.repo_id.clone(), id)
            .k1(cx.actor_id.clone())
            .n1(expires as i64);
        row.body = body(&lease, &req.org_id);
        if let Some(c) = &req.change_id
            && !c.raw.is_empty()
        {
            row = row.k2(hex(&c.raw));
        }
        tx.insert(&LEASES, &row).await?;
        Self::emit(&mut tx, "lease.acquired", &req.org_id, &lease).await?;
        tx.audit(Self::audit("lease.acquire", &req.org_id, &lease))
            .await?;
        tx.commit().await?;
        Ok((lease, overlapping))
    }

    /// Reads a live lease.
    pub async fn get(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<Lease> {
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let now = tx.now_ms();
        match tx.get_msg::<Lease>(&LEASES, repo_id, id).await? {
            Some((l, row)) if row.n1.unwrap_or(0) as u64 > now => Ok(l),
            _ => Err(ForgeError::not_found(format!("lease {id}"))),
        }
    }

    /// Live leases of a repo, optionally only those overlapping `paths`, soonest expiry first.
    pub async fn list(&self, cx: &Cx, repo_id: &str, paths: &[Vec<u8>]) -> Result<Vec<Lease>> {
        let filter = paths
            .iter()
            .map(|p| normalize_path(p))
            .collect::<Result<Vec<_>>>()?;
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let now = tx.now_ms();
        Ok(Self::live(&mut tx, repo_id, now)
            .await?
            .into_iter()
            .map(|(l, _)| l)
            .filter(|l| filter.is_empty() || paths_overlap(&filter, &lease_paths(l)))
            .collect())
    }

    /// Moves a live lease's expiry to now + `ttl_seconds` (0 = default). Holder only.
    pub async fn renew(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        id: &str,
        ttl_seconds: u32,
    ) -> Result<Lease> {
        let ttl = match ttl_seconds {
            0 => DEFAULT_TTL_SECS,
            t if t > MAX_TTL_SECS => {
                return Err(ForgeError::invalid(
                    "ttl_too_long",
                    format!("ttl_seconds is at most {MAX_TTL_SECS}"),
                ));
            }
            t => t,
        };
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let now = tx.now_ms();
        let Some((mut lease, row)) = tx.get_msg::<Lease>(&LEASES, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("lease {id}")));
        };
        if row.n1.unwrap_or(0) as u64 <= now {
            return Err(ForgeError::not_found(format!("lease {id} (expired)")));
        }
        if holder_id(&lease) != cx.actor_id {
            return Err(ForgeError::permission_denied(
                "only the holder renews a lease",
            ));
        }
        let expires = now + u64::from(ttl) * 1000;
        lease.expires_at = Some(ts(expires));
        let mut next = row.clone().n1(expires as i64);
        next.body = body(&lease, org_id);
        tx.update(&LEASES, &next).await?;
        Self::emit(&mut tx, "lease.renewed", org_id, &lease).await?;
        tx.audit(Self::audit("lease.renew", org_id, &lease)).await?;
        tx.commit().await?;
        Ok(lease)
    }

    /// Releases a lease. Only the holder may, unless `force` (maintainers breaking a lease).
    pub async fn release(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        id: &str,
        force: bool,
    ) -> Result<()> {
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let now = tx.now_ms();
        let Some((lease, row)) = tx.get_msg::<Lease>(&LEASES, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("lease {id}")));
        };
        if row.n1.unwrap_or(0) as u64 <= now {
            return Err(ForgeError::not_found(format!("lease {id} (expired)")));
        }
        if holder_id(&lease) != cx.actor_id && !force {
            return Err(ForgeError::permission_denied(
                "only the holder (or a maintainer) releases a lease",
            ));
        }
        tx.delete(&LEASES, repo_id, id).await?;
        Self::emit(&mut tx, "lease.released", org_id, &lease).await?;
        tx.audit(Self::audit(
            if force && holder_id(&lease) != cx.actor_id {
                "lease.break"
            } else {
                "lease.release"
            },
            org_id,
            &lease,
        ))
        .await?;
        tx.commit().await
    }

    /// Releases every lease tied to `change_id` (hex) in a repo; returns how many.
    pub async fn release_for_change(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        change_hex: &str,
        why: &str,
    ) -> Result<usize> {
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let rows = tx
            .scan_msgs::<Lease>(
                &LEASES,
                &Scan::scope(repo_id, MAX_LEASES_PER_REPO).key(Key::K2(change_hex.to_string())),
            )
            .await?;
        let n = rows.len();
        for (lease, row) in rows {
            tx.delete(&LEASES, repo_id, &row.id).await?;
            Self::emit(&mut tx, "lease.released", org_id, &lease).await?;
            tx.audit(
                Self::audit("lease.auto_release", org_id, &lease)
                    .before(&serde_json::json!({ "reason": why }).to_string()),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Deletes expired leases (all repos) and emits `lease.expired`; returns how many.
    pub async fn sweep_expired(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let now = tx.now_ms() as i64;
        let rows = tx
            .scan_msgs::<Lease>(
                &LEASES,
                &Scan {
                    order: crate::store::Order::N1,
                    n1_max: Some(now),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        let n = rows.len();
        for (lease, row) in rows {
            tx.delete(&LEASES, &row.scope, &row.id).await?;
            let org = OrgTag::decode(row.body.as_slice())
                .map(|t| t.org_id)
                .unwrap_or_default();
            Self::emit(&mut tx, "lease.expired", &org, &lease).await?;
            tx.audit(Self::audit("lease.expire", &org, &lease)).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// `LEASED` if an exclusive lease of another principal overlaps `paths` on `ref_name`.
    pub async fn land_check(
        &self,
        cx: &Cx,
        repo_id: &str,
        ref_name: &str,
        paths: &[String],
        lander: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(Surface::LEASES, cx).await?;
        let now = tx.now_ms();
        let conflicts: Vec<Lease> = Self::live(&mut tx, repo_id, now)
            .await?
            .into_iter()
            .map(|(l, _)| l)
            .filter(|l| {
                l.r#ref == ref_name
                    && exclusive(l)
                    && holder_id(l) != lander
                    && paths_overlap(paths, &lease_paths(l))
            })
            .collect();
        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(ForgeError::leased(
                conflicts.iter().map(|l| l.id.clone()).collect(),
                conflicts.iter().flat_map(|l| l.paths.clone()).collect(),
            ))
        }
    }

    /// [`Leases::land_check`] for the VCS land path: an unreachable lease store (or a
    /// deadline) counts as "no leases", so a forge outage never blocks a land (SPEC §15.1).
    /// `LEASED` and every other answer pass through.
    pub async fn land_check_fail_open(
        &self,
        cx: &Cx,
        repo_id: &str,
        ref_name: &str,
        paths: &[String],
        lander: &str,
    ) -> Result<()> {
        match self.land_check(cx, repo_id, ref_name, paths, lander).await {
            Err(e) if e.retryable() => Ok(()),
            other => other,
        }
    }

    /// Live leases of a ref as policy input (`bee_policy::LandContext::leases`).
    pub async fn policy_leases(
        &self,
        cx: &Cx,
        repo_id: &str,
        ref_name: &str,
    ) -> Result<Vec<bee_policy::LeaseInfo>> {
        Ok(self
            .list(cx, repo_id, &[])
            .await?
            .into_iter()
            .filter(|l| l.r#ref == ref_name)
            .map(|l| bee_policy::LeaseInfo {
                id: l.id.clone(),
                holder: bee_policy::PrincipalId::new(holder_id(&l)),
                paths: lease_paths(&l),
                exclusive: exclusive(&l),
            })
            .collect())
    }
}

/// The org id stored next to a lease. A lease row's body is the `Lease` encoding followed by
/// this message's field 100, so it decodes both as a `Lease` (unknown field skipped) and as
/// an `OrgTag`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct OrgTag {
    /// Org of the leased repo.
    #[prost(string, tag = "100")]
    pub org_id: String,
}

fn body(lease: &Lease, org_id: &str) -> Vec<u8> {
    let mut b = lease.encode_to_vec();
    let _ = OrgTag {
        org_id: org_id.to_string(),
    }
    .encode(&mut b);
    b
}

fn json(l: &Lease) -> String {
    serde_json::json!({
        "id": l.id,
        "ref": l.r#ref,
        "paths": lease_paths(l),
        "mode": if exclusive(l) { "exclusive" } else { "advisory" },
        "expires_at_ms": l.expires_at.as_ref().map(ts_ms).unwrap_or(0),
        "change_id": l.change_id.as_ref().map(|c| hex(&c.raw)).unwrap_or_default(),
    })
    .to_string()
}

/// Releases a change request's leases when it lands or closes.
pub struct AutoRelease {
    leases: Arc<Leases>,
}

impl AutoRelease {
    /// A listener over `leases`.
    pub fn new(leases: Arc<Leases>) -> AutoRelease {
        AutoRelease { leases }
    }
}

#[async_trait]
impl EventListener for AutoRelease {
    async fn on_event(&self, ev: &Event) {
        let why = match ev.kind.as_str() {
            "cr.landed" => "landed",
            "cr.closed" => "closed",
            _ => return,
        };
        let Some(event::Body::Change(cr)) = &ev.body else {
            return;
        };
        let cx = Cx::system();
        for c in &cr.change_ids {
            let _ = self
                .leases
                .release_for_change(&cx, &ev.org_id, &ev.repo_id, &hex(&c.raw), why)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn overlap_is_component_wise() {
        assert!(paths_overlap(&v(&["src"]), &v(&["src/a.rs"])));
        assert!(paths_overlap(&v(&["src/a.rs"]), &v(&["src"])));
        assert!(!paths_overlap(&v(&["src"]), &v(&["src2"])));
        assert!(paths_overlap(&v(&[""]), &v(&["docs"])));
        assert!(!paths_overlap(&v(&["docs", "a"]), &v(&["b", "src"])));
    }

    #[test]
    fn paths_normalize() {
        assert_eq!(normalize_path(b"/src/").unwrap(), "src");
        assert_eq!(normalize_path(b"/").unwrap(), "");
        assert!(normalize_path(b"a/../b").is_err());
        assert!(normalize_path(b"a//b").is_err());
        assert!(normalize_path(&[0xff]).is_err());
    }
}
