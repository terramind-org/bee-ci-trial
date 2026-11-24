//! Cost centers: named buckets that usage of their repos, orgs and principals is attributed
//! to. A resource belongs to at most one cost center per account; adding it to another center
//! moves it. Usage resolves a center from its repo, then its principal (an agent through its
//! owner), then its org.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{CostCenter, RepoRef, repo_ref};

use super::{Billing, CC_ASSIGN, COST_CENTERS, Change, SURFACE, event_org, record_change};
use crate::cx::Cx;
use crate::enterprise::common::{ResolvedScope, SCAN_CAP, get_or_404, json, scan_all, scan_page};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row, Scan, Tx};

/// Most cost centers one account holds.
pub const MAX_COST_CENTERS: usize = 1000;
/// Most resources one request adds or removes.
pub const MAX_RESOURCES_PER_REQUEST: usize = 1000;

fn repo_id_of(r: &RepoRef) -> String {
    match &r.repo {
        Some(repo_ref::Repo::Id(i)) | Some(repo_ref::Repo::FullName(i)) => i.clone(),
        None => String::new(),
    }
}

fn clean_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 100 {
        return Err(ForgeError::invalid(
            "invalid_name",
            "a cost center name is 1-100 characters",
        ));
    }
    Ok(n.to_string())
}

/// A resource kind and id as an assignment key.
fn key_of(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

fn contains(c: &CostCenter, kind: &str, id: &str) -> bool {
    match kind {
        "repo" => c.repos.iter().any(|r| repo_id_of(r) == id),
        "org" => c.orgs.iter().any(|o| o == id),
        _ => c.principal_ids.iter().any(|p| p == id),
    }
}

fn insert(c: &mut CostCenter, kind: &str, id: &str) {
    match kind {
        "repo" => {
            let mut ids: BTreeSet<String> = c.repos.iter().map(repo_id_of).collect();
            ids.insert(id.to_string());
            c.repos = ids
                .into_iter()
                .map(|i| RepoRef {
                    repo: Some(repo_ref::Repo::Id(i)),
                })
                .collect();
        }
        "org" => {
            let mut s: BTreeSet<String> = c.orgs.iter().cloned().collect();
            s.insert(id.to_string());
            c.orgs = s.into_iter().collect();
        }
        _ => {
            let mut s: BTreeSet<String> = c.principal_ids.iter().cloned().collect();
            s.insert(id.to_string());
            c.principal_ids = s.into_iter().collect();
        }
    }
}

fn remove(c: &mut CostCenter, kind: &str, id: &str) {
    match kind {
        "repo" => c.repos.retain(|r| repo_id_of(r) != id),
        "org" => c.orgs.retain(|o| o != id),
        _ => c.principal_ids.retain(|p| p != id),
    }
}

/// The resources of a request as `(kind, id)` pairs.
type Resources = Vec<(&'static str, String)>;

impl Billing {
    fn cc_view(scope: &ResolvedScope, mut c: CostCenter) -> CostCenter {
        c.scope = Some(scope.to_proto());
        c
    }

    async fn unique_name(
        tx: &mut Tx,
        scope: &ResolvedScope,
        name: &str,
        except: &str,
    ) -> Result<()> {
        let all = scan_all::<CostCenter>(tx, &COST_CENTERS, &scope.key(), None).await?;
        if all
            .iter()
            .any(|(c, _)| c.id != except && c.name.eq_ignore_ascii_case(name))
        {
            return Err(ForgeError::already_exists(format!("cost center {name}")));
        }
        Ok(())
    }

    /// Creates a cost center.
    pub async fn create_cost_center(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        name: &str,
    ) -> Result<CostCenter> {
        let name = clean_name(name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::unique_name(&mut tx, scope, &name, "").await?;
        if tx.count(&COST_CENTERS, &scope.key(), None).await? as usize >= MAX_COST_CENTERS {
            return Err(ForgeError::too_large(
                "cost_centers",
                MAX_COST_CENTERS as u64,
                MAX_COST_CENTERS as u64 + 1,
            ));
        }
        let now = ts(tx.now_ms());
        let c = CostCenter {
            id: tx.new_id()?,
            scope: None,
            name,
            repos: vec![],
            orgs: vec![],
            principal_ids: vec![],
            created_at: Some(now),
            updated_at: Some(now),
        };
        tx.insert(&COST_CENTERS, &Row::new(scope.key(), c.id.clone()).msg(&c))
            .await?;
        let out = Self::cc_view(scope, c);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.cost_center_changed",
                action: "billing.cost_center.create",
                target: &out.id,
                before: None,
                resource: &json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// One cost center.
    pub async fn get_cost_center(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> Result<CostCenter> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (c, _) =
            get_or_404::<CostCenter>(&mut tx, &COST_CENTERS, &scope.key(), id, "cost center")
                .await?;
        Ok(Self::cc_view(scope, c))
    }

    /// Renames a cost center.
    pub async fn update_cost_center(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        name: Option<String>,
    ) -> Result<CostCenter> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut c, row) =
            get_or_404::<CostCenter>(&mut tx, &COST_CENTERS, &scope.key(), id, "cost center")
                .await?;
        let before = json(&Self::cc_view(scope, c.clone()));
        if let Some(n) = name {
            let n = clean_name(&n)?;
            Self::unique_name(&mut tx, scope, &n, id).await?;
            c.name = n;
        }
        c.updated_at = Some(ts(tx.now_ms()));
        tx.update(&COST_CENTERS, &row.msg(&c)).await?;
        let out = Self::cc_view(scope, c);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.cost_center_changed",
                action: "billing.cost_center.update",
                target: id,
                before: Some(&before),
                resource: &json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Deletes a cost center; its usage reverts to the account.
    pub async fn delete_cost_center(&self, cx: &Cx, scope: &ResolvedScope, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (c, _) =
            get_or_404::<CostCenter>(&mut tx, &COST_CENTERS, &scope.key(), id, "cost center")
                .await?;
        let before = json(&Self::cc_view(scope, c));
        let key = scope.key();
        let rows = tx
            .scan(
                &CC_ASSIGN,
                &Scan::scope(key.clone(), SCAN_CAP).key(Key::K1(id.to_string())),
            )
            .await?;
        for r in rows {
            tx.delete(&CC_ASSIGN, &key, &r.id).await?;
        }
        tx.delete(&COST_CENTERS, &key, id).await?;
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.cost_center_changed",
                action: "billing.cost_center.delete",
                target: id,
                before: Some(&before),
                resource: "{}",
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Cost centers ordered by id; returns the page and the next token.
    pub async fn list_cost_centers(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<CostCenter>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<CostCenter>(&mut tx, &COST_CENTERS, &scope.key(), None, after, limit)
                .await?;
        Ok((
            rows.into_iter()
                .map(|(c, _)| Self::cc_view(scope, c))
                .collect(),
            next,
        ))
    }

    /// Resolves the resources of a request to ids of the scope.
    async fn resolve_resources(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        repos: &[RepoRef],
        orgs: &[String],
        principals: &[String],
        check_scope: bool,
    ) -> Result<Resources> {
        if repos.len() + orgs.len() + principals.len() > MAX_RESOURCES_PER_REQUEST {
            return Err(ForgeError::too_large(
                "cost_center_resources",
                MAX_RESOURCES_PER_REQUEST as u64,
                (repos.len() + orgs.len() + principals.len()) as u64,
            ));
        }
        let mut out: Resources = Vec::new();
        for r in repos {
            if r.repo.is_none() {
                return Err(ForgeError::missing("repos"));
            }
            let id = match self.repo_in_scope(cx, scope, r).await {
                Ok(id) => id,
                Err(e) if check_scope => return Err(e),
                Err(_) => repo_id_of(r),
            };
            out.push(("repo", id));
        }
        for o in orgs {
            let id = match self.org_in_scope(cx, scope, o).await {
                Ok(id) => id,
                Err(e) if check_scope => return Err(e),
                Err(_) => o.clone(),
            };
            out.push(("org", id));
        }
        for p in principals {
            if p.is_empty() {
                return Err(ForgeError::missing("principal_ids"));
            }
            out.push(("principal", p.clone()));
        }
        Ok(out)
    }

    /// Assigns repos, orgs and principals to a cost center, moving them from any other.
    pub async fn add_cost_center_resources(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        repos: &[RepoRef],
        orgs: &[String],
        principals: &[String],
    ) -> Result<CostCenter> {
        let res = self
            .resolve_resources(cx, scope, repos, orgs, principals, true)
            .await?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut c, row) =
            get_or_404::<CostCenter>(&mut tx, &COST_CENTERS, &key, id, "cost center").await?;
        let before = json(&Self::cc_view(scope, c.clone()));
        let mut others: BTreeMap<String, (CostCenter, Row, String)> = BTreeMap::new();
        for (kind, rid) in &res {
            let akey = key_of(kind, rid);
            if let Some(a) = tx.get(&CC_ASSIGN, &key, &akey).await?
                && let Some(prev) = a.k1.clone()
                && prev != id
            {
                if !others.contains_key(&prev) {
                    let (oc, orow) = get_or_404::<CostCenter>(
                        &mut tx,
                        &COST_CENTERS,
                        &key,
                        &prev,
                        "cost center",
                    )
                    .await?;
                    let b = json(&Self::cc_view(scope, oc.clone()));
                    others.insert(prev.clone(), (oc, orow, b));
                }
                if let Some((oc, _, _)) = others.get_mut(&prev) {
                    remove(oc, kind, rid);
                }
            }
            insert(&mut c, kind, rid);
            tx.put(&CC_ASSIGN, &Row::new(key.clone(), akey).k1(id))
                .await?;
        }
        let now = ts(tx.now_ms());
        for (oid, (mut oc, orow, b)) in others {
            oc.updated_at = Some(now);
            tx.update(&COST_CENTERS, &orow.msg(&oc)).await?;
            record_change(
                &mut tx,
                scope,
                Change {
                    kind: "billing.cost_center_changed",
                    action: "billing.cost_center.resources_moved_out",
                    target: &oid,
                    before: Some(&b),
                    resource: &json(&Self::cc_view(scope, oc)),
                    event_org: &event_org(scope),
                },
            )
            .await?;
        }
        c.updated_at = Some(now);
        tx.update(&COST_CENTERS, &row.msg(&c)).await?;
        let out = Self::cc_view(scope, c);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.cost_center_changed",
                action: "billing.cost_center.resources_added",
                target: id,
                before: Some(&before),
                resource: &json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Removes repos, orgs and principals from a cost center; resources it does not hold are
    /// ignored.
    pub async fn remove_cost_center_resources(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        repos: &[RepoRef],
        orgs: &[String],
        principals: &[String],
    ) -> Result<CostCenter> {
        let res = self
            .resolve_resources(cx, scope, repos, orgs, principals, false)
            .await?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut c, row) =
            get_or_404::<CostCenter>(&mut tx, &COST_CENTERS, &key, id, "cost center").await?;
        let before = json(&Self::cc_view(scope, c.clone()));
        for (kind, rid) in &res {
            if !contains(&c, kind, rid) {
                continue;
            }
            remove(&mut c, kind, rid);
            let akey = key_of(kind, rid);
            if tx
                .get(&CC_ASSIGN, &key, &akey)
                .await?
                .is_some_and(|a| a.k1.as_deref() == Some(id))
            {
                tx.delete(&CC_ASSIGN, &key, &akey).await?;
            }
        }
        c.updated_at = Some(ts(tx.now_ms()));
        tx.update(&COST_CENTERS, &row.msg(&c)).await?;
        let out = Self::cc_view(scope, c);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.cost_center_changed",
                action: "billing.cost_center.resources_removed",
                target: id,
                before: Some(&before),
                resource: &json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }
}
