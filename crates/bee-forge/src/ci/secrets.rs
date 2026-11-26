//! Secrets and variables at org, repo and environment scope.
//!
//! Secret values are sealed with the server key ([`super::SecretCipher`], ChaCha20-Poly1305,
//! bound to `<scope>/<name>`) before they reach the store, and no method returns a value
//! except [`Ci::secrets_for`], which the runner path uses to hand a running job its secrets
//! once and to mask them in logs. Org secrets and variables reach a repo by visibility: all
//! repos, private and internal repos, or a selected set. A job sees org, then repo, then
//! environment values, the more specific one winning.

use crate::safety::audit_actions; // Shared with the Safety audit consumer.
use std::collections::BTreeMap;

use bee_proto::v1::{
    Repo, Secret, SecretScope, SecretScopeKind, SecretVisibility, Variable, Visibility,
};

use super::SURFACE;
use super::engine::{Ci, repo_ref_of};
use super::model::{SecretRecord, VariableRecord};
use super::sealed::{SEAL_OVERHEAD, SealKey};
use super::{COUNTERS, SECRETS, VARIABLES};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan};

/// Largest secret value.
pub const MAX_SECRET_BYTES: usize = 48 * 1024;
/// Largest variable value.
pub const MAX_VARIABLE_BYTES: usize = 48 * 1024;
/// Most secrets or variables per scope.
pub const MAX_PER_SCOPE: usize = 1000;

/// A resolved secret or variable scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VaultScope {
    /// An org (id).
    Org(String),
    /// A repo (id, org id).
    Repo(String, String),
    /// An environment of a repo (repo id, org id, environment name).
    Environment(String, String, String),
}

impl VaultScope {
    /// The storage scope key.
    pub fn key(&self) -> String {
        match self {
            VaultScope::Org(o) => format!("org:{o}"),
            VaultScope::Repo(r, _) => format!("repo:{r}"),
            VaultScope::Environment(r, _, e) => format!("env:{r}:{}", e.to_lowercase()),
        }
    }

    /// The org.
    pub fn org_id(&self) -> &str {
        match self {
            VaultScope::Org(o) | VaultScope::Repo(_, o) | VaultScope::Environment(_, o, _) => o,
        }
    }

    fn repo_id(&self) -> &str {
        match self {
            VaultScope::Org(_) => "",
            VaultScope::Repo(r, _) | VaultScope::Environment(r, _, _) => r,
        }
    }

    fn proto(&self) -> SecretScope {
        match self {
            VaultScope::Org(o) => SecretScope {
                kind: SecretScopeKind::Org as i32,
                org: o.clone(),
                ..Default::default()
            },
            VaultScope::Repo(r, o) => SecretScope {
                kind: SecretScopeKind::Repo as i32,
                org: o.clone(),
                repo: repo_ref_of(r),
                ..Default::default()
            },
            VaultScope::Environment(r, o, e) => SecretScope {
                kind: SecretScopeKind::Environment as i32,
                org: o.clone(),
                repo: repo_ref_of(r),
                environment: e.clone(),
                ..Default::default()
            },
        }
    }
}

/// Validates and normalizes a secret or variable name (upper snake case, not `GITHUB_*`).
pub fn normalize_name(name: &str) -> Result<String> {
    let n = name.to_ascii_uppercase();
    let ok = !n.is_empty()
        && n.len() <= 200
        && n.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && n.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !n.starts_with("GITHUB_");
    if ok {
        Ok(n)
    } else {
        Err(ForgeError::invalid(
            "invalid_name",
            format!(
                "`{name}` is not a valid name: use letters, digits and `_`, not starting with a digit or GITHUB_"
            ),
        ))
    }
}

fn visible(vis: i32, selected: &[String], repo: &Repo) -> bool {
    match SecretVisibility::try_from(vis).unwrap_or(SecretVisibility::All) {
        SecretVisibility::Private => repo.visibility != Visibility::Public as i32,
        SecretVisibility::Selected => selected.contains(&repo.id),
        _ => true,
    }
}

impl Ci {
    /// The public key new values for `scope` must be sealed with (`GetSecretPublicKey`).
    pub async fn secret_public_key(&self, scope: &VaultScope) -> Result<SealKey> {
        self.sealed_box().public_key(&scope.key()).await
    }

    /// Creates or replaces a secret from a sealed box made with the scope's current public
    /// key (`PutSecret`). The box is opened in memory and the value re-sealed at rest; the box
    /// itself is never stored. Errors: INVALID_ARGUMENT `stale_key_id`, `bad_ciphertext`;
    /// TOO_LARGE.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_sealed_secret(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
        sealed: &[u8],
        key_id: &str,
        visibility: SecretVisibility,
        selected: Vec<String>,
    ) -> Result<Secret> {
        normalize_name(name)?;
        if key_id.is_empty() {
            return Err(ForgeError::missing("key_id"));
        }
        if sealed.len() > MAX_SECRET_BYTES + SEAL_OVERHEAD {
            return Err(ForgeError::too_large(
                "secret_bytes",
                MAX_SECRET_BYTES as u64,
                sealed.len().saturating_sub(SEAL_OVERHEAD) as u64,
            ));
        }
        let plain = self.sealed_box().open(&scope.key(), key_id, sealed).await?;
        if std::str::from_utf8(&plain).is_err() {
            return Err(ForgeError::invalid(
                "bad_ciphertext",
                "the sealed value is not UTF-8 text",
            ));
        }
        self.put_secret(cx, scope, name, &plain, visibility, selected)
            .await
    }

    /// Deletes every secret and variable of a scope (an environment being deleted).
    pub(crate) async fn purge_scope(&self, cx: &Cx, scope: &VaultScope) -> Result<()> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for table in [&SECRETS, &VARIABLES] {
            for row in tx
                .scan(table, &Scan::scope(key.clone(), MAX_PER_SCOPE))
                .await?
            {
                tx.delete(table, &key, &row.id).await?;
            }
        }
        tx.commit().await
    }

    /// Creates or replaces a secret. `value` is sealed before it is stored.
    pub async fn put_secret(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
        value: &[u8],
        visibility: SecretVisibility,
        selected: Vec<String>,
    ) -> Result<Secret> {
        let name = normalize_name(name)?;
        if value.len() > MAX_SECRET_BYTES {
            return Err(ForgeError::too_large(
                "secret_bytes",
                MAX_SECRET_BYTES as u64,
                value.len() as u64,
            ));
        }
        let is_org = matches!(scope, VaultScope::Org(_));
        if !is_org && visibility != SecretVisibility::Unspecified {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "visibility applies to org secrets only",
            ));
        }
        let key = scope.key();
        let (nonce, ciphertext) = self
            .cipher
            .seal(value, format!("{key}/{name}").as_bytes())?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let existing = tx.get_msg::<SecretRecord>(&SECRETS, &key, &name).await?;
        if existing.is_none() {
            tx.add_n1(&COUNTERS, &key, "secret_scope_cap", 0).await?;
        }
        if existing.is_none() && tx.count(&SECRETS, &key, None).await? as usize >= MAX_PER_SCOPE {
            return Err(ForgeError::too_large(
                "secrets_per_scope",
                MAX_PER_SCOPE as u64,
                MAX_PER_SCOPE as u64 + 1,
            ));
        }
        let now = tx.now_ms();
        let vis = if is_org && visibility == SecretVisibility::Unspecified {
            SecretVisibility::All
        } else {
            visibility
        };
        let mut selected = selected;
        selected.sort();
        selected.dedup();
        let created = existing
            .as_ref()
            .and_then(|(r, _)| r.secret.as_ref().and_then(|s| s.created_at))
            .unwrap_or(ts(now));
        let secret = Secret {
            name: name.clone(),
            scope: Some(scope.proto()),
            visibility: vis as i32,
            selected_repo_count: if vis == SecretVisibility::Selected {
                selected.len() as u32
            } else {
                0
            },
            created_at: Some(created),
            updated_at: Some(ts(now)),
            updated_by: Some(cx.principal()),
        };
        let rec = SecretRecord {
            secret: Some(secret.clone()),
            nonce,
            ciphertext,
            repo_ids: selected,
        };
        tx.put(&SECRETS, &Row::new(key.clone(), name.clone()).msg(&rec))
            .await?;
        tx.audit(
            AuditDraft::new(
                if existing.is_some() {
                    "secret.update"
                } else {
                    "secret.create"
                },
                scope.org_id(),
            )
            .repo_id(scope.repo_id())
            .target_id(&format!("{key}/{name}")),
        )
        .await?;
        tx.commit().await?;
        Ok(secret)
    }

    /// A secret's metadata (never its value).
    pub async fn get_secret(&self, cx: &Cx, scope: &VaultScope, name: &str) -> Result<Secret> {
        let name = normalize_name(name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<SecretRecord>(&SECRETS, &scope.key(), &name)
            .await?
            .and_then(|(r, _)| r.secret)
            .ok_or_else(|| ForgeError::not_found(format!("secret {name}")))
    }

    /// A secret's decrypted value, for server-side consumers that hold it by name (audit
    /// sink credentials). Never returned through an API.
    pub async fn secret_value(&self, cx: &Cx, scope: &VaultScope, name: &str) -> Result<String> {
        let name = normalize_name(name)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (r, row) = tx
            .get_msg::<SecretRecord>(&SECRETS, &key, &name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("secret {name}")))?;
        let plain = self.cipher.open(
            &r.nonce,
            &r.ciphertext,
            format!("{key}/{}", row.id).as_bytes(),
        )?;
        Ok(String::from_utf8_lossy(&plain).into_owned())
    }

    /// The secrets of a scope (metadata only), by name.
    pub async fn list_secrets(&self, cx: &Cx, scope: &VaultScope) -> Result<Vec<Secret>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<SecretRecord>(&SECRETS, &Scan::scope(scope.key(), MAX_PER_SCOPE))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.secret)
            .collect())
    }

    /// Org secrets visible to a repo (metadata only).
    pub async fn repo_org_secrets(&self, cx: &Cx, repo: &Repo) -> Result<Vec<Secret>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<SecretRecord>(
                &SECRETS,
                &Scan::scope(format!("org:{}", repo.org_id), MAX_PER_SCOPE),
            )
            .await?
            .into_iter()
            .filter(|(r, _)| {
                visible(
                    r.secret.as_ref().map(|s| s.visibility).unwrap_or(0),
                    &r.repo_ids,
                    repo,
                )
            })
            .filter_map(|(r, _)| r.secret)
            .collect())
    }

    /// Deletes a secret.
    pub async fn delete_secret(&self, cx: &Cx, scope: &VaultScope, name: &str) -> Result<()> {
        let name = normalize_name(name)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&SECRETS, &key, &name).await? {
            return Err(ForgeError::not_found(format!("secret {name}")));
        }
        tx.audit(
            AuditDraft::new("secret.delete", scope.org_id())
                .repo_id(scope.repo_id())
                .target_id(&format!("{key}/{name}")),
        )
        .await?;
        tx.commit().await
    }

    /// Replaces the selected repos of an org secret.
    pub async fn set_secret_repos(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
        repos: Vec<String>,
    ) -> Result<()> {
        let name = normalize_name(name)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx.get_msg::<SecretRecord>(&SECRETS, &key, &name).await? else {
            return Err(ForgeError::not_found(format!("secret {name}")));
        };
        let mut s = rec.secret.clone().unwrap_or_default();
        if s.visibility != SecretVisibility::Selected as i32 {
            return Err(ForgeError::invalid_state(
                "the secret's visibility is not SELECTED",
            ));
        }
        let mut repos = repos;
        repos.sort();
        repos.dedup();
        s.selected_repo_count = repos.len() as u32;
        rec.secret = Some(s);
        rec.repo_ids = repos;
        tx.update(
            &SECRETS,
            &Row::new(key.clone(), name.clone())
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        tx.audit(
            AuditDraft::new(audit_actions::SECRET_REPOS, scope.org_id())
                .target_id(&format!("{key}/{name}"))
                .after(&serde_json::json!({ "repos": rec.repo_ids }).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// Selected repo ids of an org secret.
    pub async fn secret_repos(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
    ) -> Result<Vec<String>> {
        let name = normalize_name(name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<SecretRecord>(&SECRETS, &scope.key(), &name)
            .await?
            .map(|(r, _)| r.repo_ids)
            .ok_or_else(|| ForgeError::not_found(format!("secret {name}")))
    }

    /// Decrypted secrets a job in `repo` (deploying to `env`, if any) receives.
    pub async fn secrets_for(
        &self,
        cx: &Cx,
        repo: &Repo,
        env: &str,
    ) -> Result<BTreeMap<String, String>> {
        let mut scopes = vec![
            VaultScope::Org(repo.org_id.clone()),
            VaultScope::Repo(repo.id.clone(), repo.org_id.clone()),
        ];
        if !env.is_empty() {
            scopes.push(VaultScope::Environment(
                repo.id.clone(),
                repo.org_id.clone(),
                env.to_string(),
            ));
        }
        let mut out = BTreeMap::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for s in scopes {
            let key = s.key();
            for (r, row) in tx
                .scan_msgs::<SecretRecord>(&SECRETS, &Scan::scope(key.clone(), MAX_PER_SCOPE))
                .await?
            {
                let vis = r.secret.as_ref().map(|x| x.visibility).unwrap_or(0);
                if matches!(s, VaultScope::Org(_)) && !visible(vis, &r.repo_ids, repo) {
                    continue;
                }
                let plain = self.cipher.open(
                    &r.nonce,
                    &r.ciphertext,
                    format!("{key}/{}", row.id).as_bytes(),
                )?;
                out.insert(row.id.clone(), String::from_utf8_lossy(&plain).into_owned());
            }
        }
        Ok(out)
    }

    // ----- variables --------------------------------------------------------------------------

    /// Creates a variable.
    pub async fn create_variable(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
        value: &str,
        visibility: SecretVisibility,
        selected: Vec<String>,
    ) -> Result<Variable> {
        let name = normalize_name(name)?;
        if value.len() > MAX_VARIABLE_BYTES {
            return Err(ForgeError::too_large(
                "variable_bytes",
                MAX_VARIABLE_BYTES as u64,
                value.len() as u64,
            ));
        }
        let is_org = matches!(scope, VaultScope::Org(_));
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if tx.get(&VARIABLES, &key, &name).await?.is_some() {
            return Err(ForgeError::already_exists(format!("variable {name}")));
        }
        let now = tx.now_ms();
        let vis = if is_org && visibility == SecretVisibility::Unspecified {
            SecretVisibility::All
        } else if is_org {
            visibility
        } else {
            SecretVisibility::Unspecified
        };
        let mut selected = selected;
        selected.sort();
        selected.dedup();
        let v = Variable {
            name: name.clone(),
            value: value.to_string(),
            scope: Some(scope.proto()),
            visibility: vis as i32,
            selected_repo_count: if vis == SecretVisibility::Selected {
                selected.len() as u32
            } else {
                0
            },
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            updated_by: Some(cx.principal()),
        };
        tx.insert(
            &VARIABLES,
            &Row::new(key.clone(), name.clone()).msg(&VariableRecord {
                variable: Some(v.clone()),
                repo_ids: selected,
            }),
        )
        .await?;
        tx.audit(
            AuditDraft::new("variable.create", scope.org_id())
                .repo_id(scope.repo_id())
                .target_id(&format!("{key}/{name}")),
        )
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// One variable.
    pub async fn get_variable(&self, cx: &Cx, scope: &VaultScope, name: &str) -> Result<Variable> {
        let name = normalize_name(name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<VariableRecord>(&VARIABLES, &scope.key(), &name)
            .await?
            .and_then(|(r, _)| r.variable)
            .ok_or_else(|| ForgeError::not_found(format!("variable {name}")))
    }

    /// The variables of a scope.
    pub async fn list_variables(&self, cx: &Cx, scope: &VaultScope) -> Result<Vec<Variable>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<VariableRecord>(&VARIABLES, &Scan::scope(scope.key(), MAX_PER_SCOPE))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.variable)
            .collect())
    }

    /// Updates a variable (rename, value, visibility).
    pub async fn update_variable(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
        new_name: Option<&str>,
        value: Option<&str>,
        visibility: Option<SecretVisibility>,
    ) -> Result<Variable> {
        let name = normalize_name(name)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<VariableRecord>(&VARIABLES, &key, &name)
            .await?
        else {
            return Err(ForgeError::not_found(format!("variable {name}")));
        };
        let mut v = rec.variable.clone().unwrap_or_default();
        if let Some(val) = value {
            if val.len() > MAX_VARIABLE_BYTES {
                return Err(ForgeError::too_large(
                    "variable_bytes",
                    MAX_VARIABLE_BYTES as u64,
                    val.len() as u64,
                ));
            }
            v.value = val.to_string();
        }
        if let Some(vis) = visibility {
            if !matches!(scope, VaultScope::Org(_)) {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "visibility applies to org variables only",
                ));
            }
            v.visibility = vis as i32;
        }
        v.updated_at = Some(ts(tx.now_ms()));
        v.updated_by = Some(cx.principal());
        let target = match new_name {
            Some(n) => normalize_name(n)?,
            None => name.clone(),
        };
        if target != name {
            if tx.get(&VARIABLES, &key, &target).await?.is_some() {
                return Err(ForgeError::already_exists(format!("variable {target}")));
            }
            tx.delete(&VARIABLES, &key, &name).await?;
            v.name = target.clone();
            rec.variable = Some(v.clone());
            tx.insert(&VARIABLES, &Row::new(key.clone(), target.clone()).msg(&rec))
                .await?;
        } else {
            rec.variable = Some(v.clone());
            tx.update(
                &VARIABLES,
                &Row::new(key.clone(), name.clone())
                    .version(row.version)
                    .msg(&rec),
            )
            .await?;
        }
        tx.audit(
            AuditDraft::new("variable.update", scope.org_id())
                .repo_id(scope.repo_id())
                .target_id(&format!("{key}/{target}")),
        )
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// Deletes a variable.
    pub async fn delete_variable(&self, cx: &Cx, scope: &VaultScope, name: &str) -> Result<()> {
        let name = normalize_name(name)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&VARIABLES, &key, &name).await? {
            return Err(ForgeError::not_found(format!("variable {name}")));
        }
        tx.audit(
            AuditDraft::new("variable.delete", scope.org_id())
                .repo_id(scope.repo_id())
                .target_id(&format!("{key}/{name}")),
        )
        .await?;
        tx.commit().await
    }

    /// Replaces the selected repos of an org variable.
    pub async fn set_variable_repos(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
        repos: Vec<String>,
    ) -> Result<()> {
        let name = normalize_name(name)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<VariableRecord>(&VARIABLES, &key, &name)
            .await?
        else {
            return Err(ForgeError::not_found(format!("variable {name}")));
        };
        let mut v = rec.variable.clone().unwrap_or_default();
        if v.visibility != SecretVisibility::Selected as i32 {
            return Err(ForgeError::invalid_state(
                "the variable's visibility is not SELECTED",
            ));
        }
        let mut repos = repos;
        repos.sort();
        repos.dedup();
        v.selected_repo_count = repos.len() as u32;
        rec.variable = Some(v);
        rec.repo_ids = repos;
        tx.update(
            &VARIABLES,
            &Row::new(key.clone(), name.clone())
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        tx.audit(
            AuditDraft::new("variable.set_repos", scope.org_id())
                .target_id(&format!("{key}/{name}")),
        )
        .await?;
        tx.commit().await
    }

    /// Selected repo ids of an org variable.
    pub async fn variable_repos(
        &self,
        cx: &Cx,
        scope: &VaultScope,
        name: &str,
    ) -> Result<Vec<String>> {
        let name = normalize_name(name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<VariableRecord>(&VARIABLES, &scope.key(), &name)
            .await?
            .map(|(r, _)| r.repo_ids)
            .ok_or_else(|| ForgeError::not_found(format!("variable {name}")))
    }

    /// The `vars` a job in `repo` (deploying to `env`, if any) sees.
    pub async fn vars_for(
        &self,
        cx: &Cx,
        repo: &Repo,
        env: &str,
    ) -> Result<BTreeMap<String, String>> {
        let mut scopes = vec![
            VaultScope::Org(repo.org_id.clone()),
            VaultScope::Repo(repo.id.clone(), repo.org_id.clone()),
        ];
        if !env.is_empty() {
            scopes.push(VaultScope::Environment(
                repo.id.clone(),
                repo.org_id.clone(),
                env.to_string(),
            ));
        }
        let mut out = BTreeMap::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for s in scopes {
            for (r, row) in tx
                .scan_msgs::<VariableRecord>(&VARIABLES, &Scan::scope(s.key(), MAX_PER_SCOPE))
                .await?
            {
                let v = r.variable.clone().unwrap_or_default();
                if matches!(s, VaultScope::Org(_)) && !visible(v.visibility, &r.repo_ids, repo) {
                    continue;
                }
                out.insert(row.id.clone(), v.value);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_upper_snake() {
        assert_eq!(normalize_name("api_token").unwrap(), "API_TOKEN");
        assert!(normalize_name("1X").is_err());
        assert!(normalize_name("GITHUB_TOKEN").is_err());
        assert!(normalize_name("A-B").is_err());
        assert!(normalize_name("").is_err());
    }
}
