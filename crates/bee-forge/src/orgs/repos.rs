//! Repos: create, read, rename with redirects, archive, soft delete and restore, transfer,
//! templates, topics, settings, autolinks, custom property values, collaborators and repo
//! invitations, resolver hook, mirrors and deploy keys.

use crate::safety::audit_actions; // Shared with the Safety audit consumer.
use std::collections::BTreeMap;

use bee_proto::v1::{
    Autolink, Collaborator, CollaboratorPermission, DeployKey, Event, InvitationState, KeyValue,
    LandMethod, Principal, Repo, RepoInvitation, RepoMirror, RepoRef, RepoRole, RepoSettings,
    ResolverHook, Role, SubjectKind, SubjectRef, Visibility, repo_ref,
};

use super::{
    AUTOLINKS, COLLABORATORS, DAY_MS, DEPLOY_KEYS, INVITATION_DAYS, MEMBERS, MIRRORS, ORGS, Orgs,
    PROP_DEFS, REDIRECT_DAYS, REPO_INVITES, REPO_REDIRECTS, REPO_SETTINGS, REPOS, RESOLVERS,
    RESTORE_DAYS, check_property_value, next_token, page_cursor, role_rank, validate_name,
};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Key, Row, Scan, Surface, Tx};

/// Most topics per repo.
pub const MAX_TOPICS: usize = 20;
/// Most autolinks per repo.
pub const MAX_AUTOLINKS: u64 = 500;

/// Stored resolver hook with its write-only secret.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ResolverRecord {
    /// Public view.
    #[prost(message, optional, tag = "1")]
    pub hook: Option<ResolverHook>,
    /// HMAC key.
    #[prost(string, tag = "2")]
    pub secret: String,
}

/// Stored mirror with its write-only credential.
#[derive(Clone, PartialEq, prost::Message)]
pub struct MirrorRecord {
    /// Public view.
    #[prost(message, optional, tag = "1")]
    pub mirror: Option<RepoMirror>,
    /// Remote credential.
    #[prost(string, tag = "2")]
    pub credential: String,
}

/// Fields of a new repo.
#[derive(Clone, Debug, Default)]
pub struct NewRepo {
    /// Preallocated id (the VCS side is provisioned under it first).
    pub id: String,
    /// Name.
    pub name: String,
    /// Visibility (unspecified = private).
    pub visibility: Visibility,
    /// Description.
    pub description: String,
    /// Template flag.
    pub is_template: bool,
    /// Topics.
    pub topics: Vec<String>,
    /// Homepage.
    pub homepage: String,
    /// Repo this one was created from.
    pub template_repo_id: String,
    /// The repo this one is a fork of; empty unless the repo is a fork.
    pub fork_parent_id: String,
}

/// A partial repo update.
#[derive(Clone, Debug, Default)]
pub struct RepoPatch {
    /// Rename.
    pub name: Option<String>,
    /// Visibility.
    pub visibility: Option<Visibility>,
    /// Description.
    pub description: Option<String>,
    /// Homepage.
    pub homepage: Option<String>,
    /// Default ref.
    pub default_ref: Option<String>,
    /// Template flag.
    pub is_template: Option<bool>,
}

/// Default repo settings (SPEC §6.4, §6.5, §7.4 defaults).
pub fn default_repo_settings(repo_id: &str) -> RepoSettings {
    RepoSettings {
        repo_id: repo_id.into(),
        allowed_land_methods: vec![
            LandMethod::Rebase as i32,
            LandMethod::Squash as i32,
            LandMethod::MergeCommit as i32,
        ],
        default_land_method: LandMethod::Rebase as i32,
        squash_message_template: "title_and_body".into(),
        allow_auto_merge: true,
        merge_queue_required: false,
        suggest_update_base: true,
        delete_workspace_ref_on_land: true,
        has_issues: true,
        has_projects: true,
        has_wiki: true,
        has_discussions: false,
        allow_forking: true,
        web_commit_signoff_required: false,
        draft_retention_days: 30,
        cold_after_days: 90,
        push_auto_change: false,
        git_enabled: true,
        land_attempt_budget: 0,
        auto_revert: false,
        merge_queue_check_combined: false,
        version: 1,
    }
}

fn name_key(org_id: &str, name: &str) -> String {
    format!("{org_id}/{}", name.to_lowercase())
}

fn validate_topic(t: &str) -> Result<()> {
    let ok = !t.is_empty()
        && t.len() <= 50
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !t.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(ForgeError::invalid(
            "invalid_topic",
            format!("topic {t:?} must be 1-50 of a-z 0-9 -, not starting with -"),
        ))
    }
}

fn clean_topics(topics: Vec<String>) -> Result<Vec<String>> {
    let mut t: Vec<String> = topics
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .collect();
    t.sort();
    t.dedup();
    for x in &t {
        validate_topic(x)?;
    }
    if t.len() > MAX_TOPICS {
        return Err(ForgeError::too_large(
            "topics",
            MAX_TOPICS as u64,
            t.len() as u64,
        ));
    }
    Ok(t)
}

fn repo_json(r: &Repo) -> String {
    serde_json::json!({
        "id": r.id, "name": r.name, "full_name": r.full_name,
        "visibility": Visibility::try_from(r.visibility).unwrap_or(Visibility::Unspecified).as_str_name(),
        "archived": r.archived, "is_template": r.is_template, "default_ref": r.default_ref,
        "description": r.description, "topics": r.topics,
    })
    .to_string()
}

/// Key prefix of social preview images in the blob store and in their URLs.
pub const SOCIAL_PREVIEW_PREFIX: &str = "social-previews/";
/// Largest social preview image, in bytes.
pub const SOCIAL_PREVIEW_MAX_BYTES: u64 = 1 << 20;

impl Orgs {
    async fn repo_event(tx: &mut Tx, kind: &str, repo: &Repo) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            payload_json: serde_json::to_string(repo).unwrap_or_default(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Repo as i32,
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo.id.clone())),
                }),
                id: repo.id.clone(),
            }),
            ..Default::default()
        })
        .await
    }

    async fn repo_payload_event(
        tx: &mut Tx,
        kind: &str,
        repo: &Repo,
        payload: String,
    ) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            payload_json: payload,
            ..Default::default()
        })
        .await
    }

    async fn org_name(tx: &mut Tx, org_id: &str) -> Result<String> {
        Ok(tx
            .get_msg::<bee_proto::v1::Org>(&ORGS, "", org_id)
            .await?
            .map(|(o, _)| o.name)
            .unwrap_or_default())
    }

    async fn load_repo(tx: &mut Tx, id: &str) -> Result<Option<(Repo, Row)>> {
        match tx.get_msg::<Repo>(&REPOS, "", id).await? {
            Some((mut r, row)) => {
                r.version = row.version;
                r.full_name = format!("{}/{}", Self::org_name(tx, &r.org_id).await?, r.name);
                Ok(Some((r, row)))
            }
            None => Ok(None),
        }
    }

    async fn live_repo(tx: &mut Tx, id: &str) -> Result<(Repo, Row)> {
        match Self::load_repo(tx, id).await? {
            Some((r, row)) if row.n1.is_none() => Ok((r, row)),
            _ => Err(ForgeError::not_found(format!("repo {id}"))),
        }
    }

    async fn find_repo(tx: &mut Tx, r: &RepoRef, now: u64) -> Result<Option<(Repo, Row)>> {
        match &r.repo {
            Some(repo_ref::Repo::Id(id)) => Self::load_repo(tx, id).await,
            Some(repo_ref::Repo::FullName(full)) => {
                let Some((org, name)) = full.split_once('/') else {
                    return Err(ForgeError::invalid(
                        "invalid_name",
                        "full_name must be \"org/name\"",
                    ));
                };
                let Some(org) = Self::find_org(tx, org, now).await? else {
                    return Ok(None);
                };
                let key = name_key(&org.id, name);
                let rows = tx
                    .scan(&REPOS, &Scan::scope("", 1).key(Key::K2(key.clone())))
                    .await?;
                if let Some(row) = rows.first() {
                    let id = row.id.clone();
                    return Self::load_repo(tx, &id).await;
                }
                if let Some(redirect) = tx.get(&REPO_REDIRECTS, "", &key).await?
                    && redirect.n1.unwrap_or(0) as u64 > now
                    && let Some(target) = redirect.k1.clone()
                {
                    return Self::load_repo(tx, &target).await;
                }
                Ok(None)
            }
            None => Err(ForgeError::missing("repo")),
        }
    }

    /// A live repo by id or full name (following rename and transfer redirects). Repos of a
    /// deleted org are not found.
    pub async fn resolve_repo(&self, cx: &Cx, r: &RepoRef) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        let what = || match &r.repo {
            Some(repo_ref::Repo::Id(i)) | Some(repo_ref::Repo::FullName(i)) => i.clone(),
            None => String::new(),
        };
        match Self::find_repo(&mut tx, r, now).await? {
            Some((repo, row)) if row.n1.is_none() => {
                let org_live = tx
                    .get(&ORGS, "", &repo.org_id)
                    .await?
                    .is_some_and(|o| o.n1.is_none());
                if org_live {
                    Ok(repo)
                } else {
                    Err(ForgeError::not_found(format!("repo {}", what())))
                }
            }
            _ => Err(ForgeError::not_found(format!("repo {}", what()))),
        }
    }

    /// A repo by id, including soft-deleted ones.
    pub async fn repo_any(&self, cx: &Cx, id: &str) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Self::load_repo(&mut tx, id)
            .await?
            .map(|(r, _)| r)
            .ok_or_else(|| ForgeError::not_found(format!("repo {id}")))
    }

    /// Creates a repo (the caller provisioned the VCS side under `new.id`).
    pub async fn create_repo(&self, cx: &Cx, org_id: &str, new: NewRepo) -> Result<Repo> {
        validate_name(&new.name)?;
        let topics = clean_topics(new.topics)?;
        if new.id.is_empty() {
            return Err(ForgeError::missing("id"));
        }
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let key = name_key(org_id, &new.name);
        if !tx
            .scan(&REPOS, &Scan::scope("", 1).key(Key::K2(key.clone())))
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("repo {}", new.name)));
        }
        let now = tx.now_ms();
        let org_name = Self::org_name(&mut tx, org_id).await?;
        let repo = Repo {
            id: new.id.clone(),
            org_id: org_id.into(),
            name: new.name.clone(),
            full_name: format!("{org_name}/{}", new.name),
            visibility: if new.visibility == Visibility::Unspecified {
                Visibility::Private as i32
            } else {
                new.visibility as i32
            },
            default_ref: "main".into(),
            description: new.description,
            created_at: Some(ts(now)),
            is_template: new.is_template,
            template_repo_id: new.template_repo_id,
            fork_parent_id: new.fork_parent_id,
            topics,
            homepage: new.homepage,
            version: 1,
            ..Default::default()
        };
        tx.insert(
            &REPOS,
            &Row::new("", repo.id.clone())
                .k1(org_id)
                .k2(key.clone())
                .msg(&repo),
        )
        .await?;
        tx.delete(&REPO_REDIRECTS, "", &key).await?;
        tx.insert(
            &REPO_SETTINGS,
            &Row::new("", repo.id.clone()).msg(&default_repo_settings(&repo.id)),
        )
        .await?;
        Self::repo_event(&mut tx, "repo.created", &repo).await?;
        tx.audit(
            AuditDraft::new("repo.create", org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Repo, &repo.id)
                .after(&repo_json(&repo)),
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    async fn list_repo_rows(
        &self,
        cx: &Cx,
        org_id: &str,
        deleted: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Repo>, String)> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let rows = tx
            .scan(
                &REPOS,
                &Scan::scope("", crate::store::MAX_SCAN)
                    .key(Key::K1(org_id.into()))
                    .after(page_cursor("", after)),
            )
            .await?;
        let ids: Vec<String> = rows
            .into_iter()
            .filter(|r| r.n1.is_some() == deleted)
            .map(|r| r.id)
            .take(limit + 1)
            .collect();
        let more = ids.len() > limit;
        let mut out = Vec::new();
        for id in ids.into_iter().take(limit) {
            if let Some((r, _)) = Self::load_repo(&mut tx, &id).await? {
                out.push(r);
            }
        }
        let next = next_token(&out, more, |r| r.id.clone());
        Ok((out, next))
    }

    /// Live repos of an org by id, paged.
    pub async fn list_repos(
        &self,
        cx: &Cx,
        org_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Repo>, String)> {
        self.list_repo_rows(cx, org_id, false, after, limit).await
    }

    /// Soft-deleted repos of an org, paged.
    pub async fn list_deleted_repos(
        &self,
        cx: &Cx,
        org_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Repo>, String)> {
        self.list_repo_rows(cx, org_id, true, after, limit).await
    }

    async fn rename_in_tx(
        tx: &mut Tx,
        repo: &mut Repo,
        row: &mut Row,
        new_name: &str,
        new_org: &str,
    ) -> Result<()> {
        validate_name(new_name)?;
        let key = name_key(new_org, new_name);
        let old_key = name_key(&repo.org_id, &repo.name);
        if key != old_key {
            let taken = tx
                .scan(&REPOS, &Scan::scope("", 1).key(Key::K2(key.clone())))
                .await?;
            if taken.iter().any(|r| r.id != repo.id) {
                return Err(ForgeError::already_exists(format!("repo {new_name}")));
            }
            let now = tx.now_ms();
            tx.delete(&REPO_REDIRECTS, "", &key).await?;
            tx.put(
                &REPO_REDIRECTS,
                &Row::new("", old_key)
                    .k1(repo.id.clone())
                    .n1((now + REDIRECT_DAYS * DAY_MS) as i64),
            )
            .await?;
        }
        repo.name = new_name.into();
        repo.org_id = new_org.into();
        row.k1 = Some(new_org.into());
        row.k2 = Some(key);
        Ok(())
    }

    /// Updates repo metadata, renaming when `name` changes (CAS unless `expected_version` is 0).
    pub async fn update_repo(
        &self,
        cx: &Cx,
        id: &str,
        p: RepoPatch,
        expected_version: u64,
    ) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (mut repo, mut row) = Self::live_repo(&mut tx, id).await?;
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        let before = repo_json(&repo);
        let mut renamed = false;
        if let Some(n) = p.name.filter(|n| *n != repo.name) {
            let org = repo.org_id.clone();
            Self::rename_in_tx(&mut tx, &mut repo, &mut row, &n, &org).await?;
            renamed = true;
        }
        if let Some(v) = p.visibility.filter(|v| *v != Visibility::Unspecified) {
            repo.visibility = v as i32;
        }
        if let Some(d) = p.description {
            repo.description = d;
        }
        if let Some(h) = p.homepage {
            repo.homepage = h;
        }
        if let Some(d) = p.default_ref {
            if d.is_empty() {
                return Err(ForgeError::invalid(
                    "invalid_ref",
                    "default_ref cannot be empty",
                ));
            }
            repo.default_ref = d;
        }
        if let Some(t) = p.is_template {
            repo.is_template = t;
        }
        repo.version = tx.update(&REPOS, &row.clone().msg(&repo)).await?;
        repo.full_name = format!(
            "{}/{}",
            Self::org_name(&mut tx, &repo.org_id).await?,
            repo.name
        );
        Self::repo_event(
            &mut tx,
            if renamed {
                "repo.renamed"
            } else {
                "repo.updated"
            },
            &repo,
        )
        .await?;
        tx.audit(
            AuditDraft::new(
                if renamed {
                    "repo.rename"
                } else {
                    "repo.update"
                },
                &repo.org_id,
            )
            .repo_id(id)
            .target(SubjectKind::Repo, id)
            .before(&before)
            .after(&repo_json(&repo)),
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    async fn set_archived(&self, cx: &Cx, id: &str, archived: bool) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (mut repo, row) = Self::live_repo(&mut tx, id).await?;
        if repo.archived == archived {
            return Err(ForgeError::invalid_state(if archived {
                "repo is already archived"
            } else {
                "repo is not archived"
            }));
        }
        repo.archived = archived;
        repo.version = tx.update(&REPOS, &row.clone().msg(&repo)).await?;
        let (kind, action) = if archived {
            ("repo.archived", "repo.archive")
        } else {
            ("repo.unarchived", "repo.unarchive")
        };
        Self::repo_event(&mut tx, kind, &repo).await?;
        tx.audit(
            AuditDraft::new(action, &repo.org_id)
                .repo_id(id)
                .target(SubjectKind::Repo, id),
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    /// Archives a repo (read-only: writes, lands and leases are refused with `ARCHIVED`).
    pub async fn archive_repo(&self, cx: &Cx, id: &str) -> Result<Repo> {
        self.set_archived(cx, id, true).await
    }

    /// Unarchives a repo.
    pub async fn unarchive_repo(&self, cx: &Cx, id: &str) -> Result<Repo> {
        self.set_archived(cx, id, false).await
    }

    /// Soft-deletes a repo (its name is released); returns the end of the restore window.
    pub async fn delete_repo(&self, cx: &Cx, id: &str, confirm_full_name: &str) -> Result<u64> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (mut repo, row) = Self::live_repo(&mut tx, id).await?;
        if confirm_full_name != repo.full_name {
            return Err(ForgeError::invalid(
                "confirmation_mismatch",
                "confirm_full_name must equal \"org/name\"",
            ));
        }
        let now = tx.now_ms();
        repo.deleted_at = Some(ts(now));
        let mut r = row.clone().n1(now as i64).msg(&repo);
        r.k2 = None;
        tx.update(&REPOS, &r).await?;
        Self::repo_event(&mut tx, "repo.deleted", &repo).await?;
        tx.audit(
            AuditDraft::new("repo.delete", &repo.org_id)
                .repo_id(id)
                .target(SubjectKind::Repo, id)
                .before(&repo_json(&repo)),
        )
        .await?;
        tx.commit().await?;
        Ok(now + RESTORE_DAYS * DAY_MS)
    }

    /// Restores a soft-deleted repo; `ALREADY_EXISTS` if its name was taken meanwhile.
    pub async fn restore_repo(&self, cx: &Cx, id: &str) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let Some((mut repo, row)) = Self::load_repo(&mut tx, id).await? else {
            return Err(ForgeError::not_found(format!("repo {id}")));
        };
        let Some(deleted) = row.n1 else {
            return Err(ForgeError::invalid_state("repo is not deleted"));
        };
        if tx.now_ms() > deleted as u64 + RESTORE_DAYS * DAY_MS {
            return Err(ForgeError::not_found(format!(
                "repo {id} (restore window passed)"
            )));
        }
        let key = name_key(&repo.org_id, &repo.name);
        if !tx
            .scan(&REPOS, &Scan::scope("", 1).key(Key::K2(key.clone())))
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!(
                "repo {} (name taken since deletion)",
                repo.name
            )));
        }
        repo.deleted_at = None;
        let mut r = row.clone().k2(key).msg(&repo);
        r.n1 = None;
        repo.version = tx.update(&REPOS, &r).await?;
        Self::repo_event(&mut tx, "repo.restored", &repo).await?;
        tx.audit(
            AuditDraft::new("repo.restore", &repo.org_id)
                .repo_id(id)
                .target(SubjectKind::Repo, id),
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    /// Moves a repo to another org (keeping its id); the old full name redirects.
    pub async fn transfer_repo(
        &self,
        cx: &Cx,
        id: &str,
        new_org_id: &str,
        new_name: &str,
    ) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (mut repo, mut row) = Self::live_repo(&mut tx, id).await?;
        if !tx
            .get(&ORGS, "", new_org_id)
            .await?
            .is_some_and(|o| o.n1.is_none())
        {
            return Err(ForgeError::not_found(format!("org {new_org_id}")));
        }
        let before = repo_json(&repo);
        let old_org = repo.org_id.clone();
        let name = if new_name.is_empty() {
            repo.name.clone()
        } else {
            new_name.to_string()
        };
        Self::rename_in_tx(&mut tx, &mut repo, &mut row, &name, new_org_id).await?;
        repo.version = tx.update(&REPOS, &row.clone().msg(&repo)).await?;
        repo.full_name = format!(
            "{}/{}",
            Self::org_name(&mut tx, new_org_id).await?,
            repo.name
        );
        Self::repo_event(&mut tx, "repo.transferred", &repo).await?;
        let draft = |org: &str| {
            AuditDraft::new("repo.transfer", org)
                .repo_id(id)
                .target(SubjectKind::Repo, id)
                .before(&before)
                .after(&repo_json(&repo))
        };
        tx.audit(draft(&old_org)).await?;
        tx.audit(draft(new_org_id)).await?;
        tx.commit().await?;
        Ok(repo)
    }

    /// Replaces a repo's topics.
    pub async fn set_topics(&self, cx: &Cx, id: &str, topics: Vec<String>) -> Result<Vec<String>> {
        let topics = clean_topics(topics)?;
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (mut repo, row) = Self::live_repo(&mut tx, id).await?;
        let before = serde_json::json!(repo.topics).to_string();
        repo.topics = topics.clone();
        tx.update(&REPOS, &row.clone().msg(&repo)).await?;
        let after = serde_json::json!(topics).to_string();
        Self::repo_payload_event(&mut tx, "repo.topics_updated", &repo, after.clone()).await?;
        tx.audit(
            AuditDraft::new("repo.topics_update", &repo.org_id)
                .repo_id(id)
                .before(&before)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(topics)
    }

    /// Repo settings.
    pub async fn repo_settings(&self, cx: &Cx, id: &str) -> Result<RepoSettings> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(
            match tx.get_msg::<RepoSettings>(&REPO_SETTINGS, "", id).await? {
                Some((mut s, r)) => {
                    s.version = r.version;
                    s
                }
                None => default_repo_settings(id),
            },
        )
    }

    /// Edits repo settings with `f` (CAS unless `expected_version` is 0).
    pub async fn update_repo_settings(
        &self,
        cx: &Cx,
        id: &str,
        expected_version: u64,
        f: impl FnOnce(&mut RepoSettings) -> Result<()> + Send,
    ) -> Result<RepoSettings> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, id).await?;
        let (mut s, row) = match tx.get_msg::<RepoSettings>(&REPO_SETTINGS, "", id).await? {
            Some(x) => x,
            None => {
                let s = default_repo_settings(id);
                let row = Row::new("", id).msg(&s);
                tx.insert(&REPO_SETTINGS, &row).await?;
                (s, row.version(1))
            }
        };
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let before = serde_json::to_string(&s).unwrap_or_default();
        f(&mut s)?;
        if !s.allowed_land_methods.is_empty()
            && !s.allowed_land_methods.contains(&s.default_land_method)
        {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "default_land_method must be one of allowed_land_methods",
            ));
        }
        s.repo_id = id.into();
        s.version = tx.update(&REPO_SETTINGS, &row.clone().msg(&s)).await?;
        let after = serde_json::to_string(&s).unwrap_or_default();
        Self::repo_payload_event(&mut tx, "repo.settings_updated", &repo, after.clone()).await?;
        tx.audit(
            AuditDraft::new("repo.settings_update", &repo.org_id)
                .repo_id(id)
                .before(&before)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Adds an autolink.
    pub async fn create_autolink(
        &self,
        cx: &Cx,
        repo_id: &str,
        key_prefix: &str,
        url_template: &str,
        alphanumeric: bool,
    ) -> Result<Autolink> {
        if key_prefix.is_empty() {
            return Err(ForgeError::missing("key_prefix"));
        }
        if !url_template.contains("<num>") {
            return Err(ForgeError::invalid(
                "invalid_url_template",
                "url_template must contain <num>",
            ));
        }
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        if tx.count(&AUTOLINKS, repo_id, None).await? >= MAX_AUTOLINKS {
            return Err(ForgeError::too_large(
                "autolinks",
                MAX_AUTOLINKS,
                MAX_AUTOLINKS + 1,
            ));
        }
        let lower = key_prefix.to_lowercase();
        let clash = tx
            .scan(
                &AUTOLINKS,
                &Scan::scope(repo_id, 1).key(Key::K1(lower.clone())),
            )
            .await?;
        if !clash.is_empty() {
            return Err(ForgeError::already_exists(format!("autolink {key_prefix}")));
        }
        let a = Autolink {
            id: tx.new_id()?,
            key_prefix: key_prefix.into(),
            url_template: url_template.into(),
            alphanumeric,
        };
        tx.insert(
            &AUTOLINKS,
            &Row::new(repo_id, a.id.clone()).k1(lower).msg(&a),
        )
        .await?;
        let after = serde_json::to_string(&a).unwrap_or_default();
        Self::repo_payload_event(&mut tx, "autolink.created", &repo, after.clone()).await?;
        tx.audit(
            AuditDraft::new("autolink.create", &repo.org_id)
                .repo_id(repo_id)
                .target_id(&a.id)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// A repo's autolinks.
    pub async fn autolinks(&self, cx: &Cx, repo_id: &str) -> Result<Vec<Autolink>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(tx
            .scan_msgs::<Autolink>(&AUTOLINKS, &Scan::scope(repo_id, MAX_AUTOLINKS as usize))
            .await?
            .into_iter()
            .map(|(a, _)| a)
            .collect())
    }

    /// Removes an autolink.
    pub async fn delete_autolink(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        if !tx.delete(&AUTOLINKS, repo_id, id).await? {
            return Err(ForgeError::not_found(format!("autolink {id}")));
        }
        Self::repo_payload_event(
            &mut tx,
            "autolink.deleted",
            &repo,
            serde_json::json!({"id": id}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("autolink.delete", &repo.org_id)
                .repo_id(repo_id)
                .target_id(id),
        )
        .await?;
        tx.commit().await
    }

    /// Custom property values of a repo, with org defaults filled in.
    pub async fn repo_properties(&self, cx: &Cx, repo_id: &str) -> Result<Vec<KeyValue>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        let defs = tx
            .scan_msgs::<bee_proto::v1::CustomPropertyDefinition>(
                &PROP_DEFS,
                &Scan::scope(repo.org_id.clone(), 1000),
            )
            .await?;
        let mut vals: BTreeMap<String, String> = repo
            .custom_properties
            .iter()
            .map(|kv| (kv.key.clone(), kv.value.clone()))
            .collect();
        for (d, _) in defs {
            if !d.default_value.is_empty() {
                vals.entry(d.name.clone()).or_insert(d.default_value);
            }
        }
        Ok(vals
            .into_iter()
            .map(|(key, value)| KeyValue { key, value })
            .collect())
    }

    /// Sets property values (an empty value clears); every key needs an org definition.
    pub async fn set_repo_properties(
        &self,
        cx: &Cx,
        repo_id: &str,
        props: Vec<KeyValue>,
    ) -> Result<Vec<KeyValue>> {
        {
            let mut tx = self.db().begin(Surface::ORGS, cx).await?;
            let (mut repo, row) = Self::live_repo(&mut tx, repo_id).await?;
            let defs: BTreeMap<String, bee_proto::v1::CustomPropertyDefinition> = tx
                .scan_msgs::<bee_proto::v1::CustomPropertyDefinition>(
                    &PROP_DEFS,
                    &Scan::scope(repo.org_id.clone(), 1000),
                )
                .await?
                .into_iter()
                .map(|(d, _)| (d.name.clone(), d))
                .collect();
            let mut vals: BTreeMap<String, String> = repo
                .custom_properties
                .iter()
                .map(|kv| (kv.key.clone(), kv.value.clone()))
                .collect();
            let before = serde_json::json!(vals).to_string();
            for kv in props {
                let Some(def) = defs.get(&kv.key) else {
                    return Err(ForgeError::invalid(
                        "unknown_property",
                        format!("no custom property {:?} in the org", kv.key),
                    ));
                };
                if kv.value.is_empty() {
                    if def.required {
                        return Err(ForgeError::invalid(
                            "required_property",
                            format!("{} is required", kv.key),
                        ));
                    }
                    vals.remove(&kv.key);
                } else {
                    check_property_value(def, &kv.value)?;
                    vals.insert(kv.key, kv.value);
                }
            }
            repo.custom_properties = vals
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect();
            tx.update(&REPOS, &row.clone().msg(&repo)).await?;
            let after = serde_json::json!(vals).to_string();
            Self::repo_payload_event(&mut tx, "repo.properties_updated", &repo, after.clone())
                .await?;
            tx.audit(
                AuditDraft::new("repo.properties_update", &repo.org_id)
                    .repo_id(repo_id)
                    .before(&before)
                    .after(&after),
            )
            .await?;
            tx.commit().await?;
        }
        self.repo_properties(cx, repo_id).await
    }

    /// Grants `role` directly to an org member, or invites a non-member (outside
    /// collaborator). Returns exactly one of the two.
    pub async fn add_collaborator(
        &self,
        cx: &Cx,
        repo_id: &str,
        principal: Principal,
        role: RepoRole,
        custom_role_id: &str,
    ) -> Result<(Option<Collaborator>, Option<RepoInvitation>)> {
        if role == RepoRole::Unspecified && custom_role_id.is_empty() {
            return Err(ForgeError::missing("role"));
        }
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        let is_member = tx
            .get(&MEMBERS, &repo.org_id, &principal.id)
            .await?
            .is_some();
        let now = tx.now_ms();
        let result = if is_member {
            let c = Collaborator {
                principal: Some(principal.clone()),
                role: role as i32,
                custom_role_id: custom_role_id.into(),
                outside_collaborator: false,
            };
            tx.put(
                &COLLABORATORS,
                &Row::new(repo_id, principal.id.clone())
                    .k2(principal.id.clone())
                    .msg(&c),
            )
            .await?;
            let after =
                serde_json::json!({"principal_id": principal.id, "role": role.as_str_name()})
                    .to_string();
            Self::repo_payload_event(&mut tx, "collaborator.added", &repo, after.clone()).await?;
            tx.audit(
                AuditDraft::new(audit_actions::COLLABORATOR_ADD, &repo.org_id)
                    .repo_id(repo_id)
                    .target(SubjectKind::User, &principal.id)
                    .after(&after),
            )
            .await?;
            (Some(c), None)
        } else {
            let inv = RepoInvitation {
                id: tx.new_id()?,
                repo_id: repo_id.into(),
                invitee: Some(principal.clone()),
                inviter: Some(cx.principal()),
                role: role as i32,
                state: InvitationState::Pending as i32,
                created_at: Some(ts(now)),
                expires_at: Some(ts(now + INVITATION_DAYS * DAY_MS)),
            };
            tx.insert(
                &REPO_INVITES,
                &Row::new("", inv.id.clone())
                    .k1(repo_id)
                    .k2(principal.id.clone())
                    .n1((now + INVITATION_DAYS * DAY_MS) as i64)
                    .msg(&inv),
            )
            .await?;
            let after = serde_json::to_string(&inv).unwrap_or_default();
            Self::repo_payload_event(&mut tx, "repo_invitation.created", &repo, after.clone())
                .await?;
            tx.audit(
                AuditDraft::new("repo_invitation.create", &repo.org_id)
                    .repo_id(repo_id)
                    .target_id(&inv.id)
                    .after(&after),
            )
            .await?;
            (None, Some(inv))
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Removes a direct grant.
    pub async fn remove_collaborator(
        &self,
        cx: &Cx,
        repo_id: &str,
        principal_id: &str,
    ) -> Result<()> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        if !tx.delete(&COLLABORATORS, repo_id, principal_id).await? {
            return Err(ForgeError::not_found(format!(
                "collaborator {principal_id}"
            )));
        }
        Self::repo_payload_event(
            &mut tx,
            "collaborator.removed",
            &repo,
            serde_json::json!({"principal_id": principal_id}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new(audit_actions::COLLABORATOR_REMOVE, &repo.org_id)
                .repo_id(repo_id)
                .target(SubjectKind::User, principal_id),
        )
        .await?;
        tx.commit().await
    }

    /// Direct collaborators (and, unless `direct_only`, members through the org base role and
    /// owners/admins), by principal id.
    pub async fn collaborators(
        &self,
        cx: &Cx,
        repo_id: &str,
        direct_only: bool,
    ) -> Result<Vec<Collaborator>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        let mut out: BTreeMap<String, Collaborator> = BTreeMap::new();
        for (mut c, r) in tx
            .scan_msgs::<Collaborator>(
                &COLLABORATORS,
                &Scan::scope(repo_id, crate::store::MAX_SCAN),
            )
            .await?
        {
            c.outside_collaborator = tx.get(&MEMBERS, &repo.org_id, &r.id).await?.is_none();
            out.insert(r.id.clone(), c);
        }
        if !direct_only {
            let base = tx
                .get_msg::<bee_proto::v1::OrgSettings>(&super::ORG_SETTINGS, "", &repo.org_id)
                .await?
                .map(|(s, _)| RepoRole::try_from(s.base_repo_role).unwrap_or(RepoRole::Read))
                .unwrap_or(RepoRole::Read);
            for (m, r) in tx
                .scan_msgs::<bee_proto::v1::Member>(
                    &MEMBERS,
                    &Scan::scope(repo.org_id.clone(), crate::store::MAX_SCAN),
                )
                .await?
            {
                let role = match Role::try_from(m.role).unwrap_or(Role::Member) {
                    Role::Owner | Role::Admin => RepoRole::Admin,
                    _ => base,
                };
                let e = out.entry(r.id.clone()).or_insert_with(|| Collaborator {
                    principal: m.principal.clone(),
                    role: RepoRole::None as i32,
                    custom_role_id: String::new(),
                    outside_collaborator: false,
                });
                if role_rank(role) > role_rank(RepoRole::try_from(e.role).unwrap_or(RepoRole::None))
                {
                    e.role = role as i32;
                }
            }
        }
        Ok(out.into_values().collect())
    }

    /// A principal's effective role on a repo and every source contributing to it.
    pub async fn effective_role(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<CollaboratorPermission> {
        let mut grants: Vec<(RepoRole, String)> = Vec::new();
        let principal = {
            let mut tx = self.db().begin(Surface::ORGS, cx).await?;
            let member = tx
                .get_msg::<bee_proto::v1::Member>(&MEMBERS, &repo.org_id, principal_id)
                .await?;
            let mut principal = Principal {
                id: principal_id.into(),
                ..Default::default()
            };
            if let Some((m, _)) = &member {
                if let Some(p) = &m.principal {
                    principal = p.clone();
                }
                match Role::try_from(m.role).unwrap_or(Role::Member) {
                    Role::Owner => grants.push((RepoRole::Admin, "org:owner".into())),
                    Role::Admin => grants.push((RepoRole::Admin, "org:admin".into())),
                    _ => {
                        let base = tx
                            .get_msg::<bee_proto::v1::OrgSettings>(
                                &super::ORG_SETTINGS,
                                "",
                                &repo.org_id,
                            )
                            .await?
                            .map(|(s, _)| {
                                RepoRole::try_from(s.base_repo_role).unwrap_or(RepoRole::Read)
                            })
                            .unwrap_or(RepoRole::Read);
                        grants.push((base, "org:base".into()));
                    }
                }
            }
            if let Some((c, _)) = tx
                .get_msg::<Collaborator>(&COLLABORATORS, &repo.id, principal_id)
                .await?
            {
                if member.is_none()
                    && let Some(p) = c.principal.clone()
                {
                    principal = p;
                }
                grants.push((
                    RepoRole::try_from(c.role).unwrap_or(RepoRole::None),
                    "direct".into(),
                ));
            }
            // A deploy key acts as its own principal (`deploy-key:<id>`) on its repo only:
            // read, or write when the key is not read-only.
            if let Some(key_id) = principal_id.strip_prefix("deploy-key:")
                && let Some((k, _)) = tx
                    .get_msg::<DeployKey>(&DEPLOY_KEYS, &repo.id, key_id)
                    .await?
            {
                let role = if k.read_only {
                    RepoRole::Read
                } else {
                    RepoRole::Write
                };
                grants.push((role, "deploy_key".into()));
            }
            match Visibility::try_from(repo.visibility).unwrap_or(Visibility::Private) {
                Visibility::Public => grants.push((RepoRole::Read, "public".into())),
                Visibility::Internal if member.is_some() => {
                    grants.push((RepoRole::Read, "internal".into()))
                }
                _ => {}
            }
            principal
        };
        let sources: Vec<std::sync::Arc<dyn super::AccessSource>> =
            self.sources.read().map(|v| v.clone()).unwrap_or_default();
        for s in sources {
            grants.extend(s.grants(cx, repo, principal_id).await?);
        }
        let role = grants
            .iter()
            .map(|(r, _)| *r)
            .max_by_key(|r| role_rank(*r))
            .unwrap_or(RepoRole::None);
        let role = if role_rank(role) == 0 {
            RepoRole::None
        } else {
            role
        };
        Ok(CollaboratorPermission {
            principal: Some(principal),
            role: role as i32,
            sources: grants
                .into_iter()
                .filter(|(r, _)| role_rank(*r) > 0)
                .map(|(_, s)| s)
                .collect(),
        })
    }

    /// Repo invitations of a repo (`repo_id` set) or addressed to `invitee_id`.
    pub async fn repo_invitations(
        &self,
        cx: &Cx,
        repo_id: Option<&str>,
        invitee_id: &str,
    ) -> Result<Vec<RepoInvitation>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        let key = match repo_id {
            Some(r) => Key::K1(r.into()),
            None => Key::K2(invitee_id.into()),
        };
        Ok(tx
            .scan_msgs::<RepoInvitation>(&REPO_INVITES, &Scan::scope("", 1000).key(key))
            .await?
            .into_iter()
            .map(|(mut i, r)| {
                if i.state == InvitationState::Pending as i32 && r.n1.unwrap_or(0) as u64 <= now {
                    i.state = InvitationState::Expired as i32;
                }
                i
            })
            .collect())
    }

    /// Accepts or declines a repo invitation addressed to `principal`.
    pub async fn answer_repo_invitation(
        &self,
        cx: &Cx,
        id: &str,
        principal: &Principal,
        accept: bool,
    ) -> Result<Option<Collaborator>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        let Some((mut inv, row)) = tx.get_msg::<RepoInvitation>(&REPO_INVITES, "", id).await?
        else {
            return Err(ForgeError::not_found(format!("invitation {id}")));
        };
        if row.k2.as_deref() != Some(principal.id.as_str()) {
            return Err(ForgeError::not_found(format!("invitation {id}")));
        }
        if inv.state != InvitationState::Pending as i32 || row.n1.unwrap_or(0) as u64 <= now {
            return Err(ForgeError::invalid_state("invitation is no longer pending"));
        }
        let (repo, _) = Self::live_repo(&mut tx, &inv.repo_id).await?;
        inv.state = if accept {
            InvitationState::Accepted
        } else {
            InvitationState::Declined
        } as i32;
        tx.update(&REPO_INVITES, &row.clone().msg(&inv)).await?;
        let collab = if accept {
            let c = Collaborator {
                principal: Some(principal.clone()),
                role: inv.role,
                custom_role_id: String::new(),
                outside_collaborator: true,
            };
            tx.put(
                &COLLABORATORS,
                &Row::new(inv.repo_id.clone(), principal.id.clone())
                    .k2(principal.id.clone())
                    .msg(&c),
            )
            .await?;
            Some(c)
        } else {
            None
        };
        let (kind, action) = if accept {
            ("repo_invitation.accepted", "repo_invitation.accept")
        } else {
            ("repo_invitation.declined", "repo_invitation.decline")
        };
        Self::repo_payload_event(
            &mut tx,
            kind,
            &repo,
            serde_json::to_string(&inv).unwrap_or_default(),
        )
        .await?;
        tx.audit(
            AuditDraft::new(action, &repo.org_id)
                .repo_id(&repo.id)
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        Ok(collab)
    }

    /// Sets the resolver hook (`url` must be HTTPS when set).
    pub async fn set_resolver(
        &self,
        cx: &Cx,
        repo_id: &str,
        hook: ResolverHook,
        secret: &str,
    ) -> Result<ResolverHook> {
        if !hook.url.is_empty() && !hook.url.starts_with("https://") {
            return Err(ForgeError::invalid(
                "bad_url",
                "resolver url must be https://",
            ));
        }
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        let hook = ResolverHook {
            repo_id: repo_id.into(),
            timeout_seconds: if hook.timeout_seconds == 0 {
                60
            } else {
                hook.timeout_seconds.min(3600)
            },
            ..hook
        };
        tx.put(
            &RESOLVERS,
            &Row::new("", repo_id).msg(&ResolverRecord {
                hook: Some(hook.clone()),
                secret: secret.into(),
            }),
        )
        .await?;
        let after = serde_json::to_string(&hook).unwrap_or_default();
        Self::repo_payload_event(&mut tx, "resolver_hook.updated", &repo, after.clone()).await?;
        tx.audit(
            AuditDraft::new("resolver_hook.set", &repo.org_id)
                .repo_id(repo_id)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(hook)
    }

    /// The resolver hook (empty when none).
    pub async fn resolver(&self, cx: &Cx, repo_id: &str) -> Result<ResolverHook> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(tx
            .get_msg::<ResolverRecord>(&RESOLVERS, "", repo_id)
            .await?
            .and_then(|(r, _)| r.hook)
            .unwrap_or(ResolverHook {
                repo_id: repo_id.into(),
                ..Default::default()
            }))
    }

    /// Adds a mirror configuration.
    pub async fn create_mirror(
        &self,
        cx: &Cx,
        repo_id: &str,
        mirror: RepoMirror,
        credential: &str,
    ) -> Result<RepoMirror> {
        let insecure = self.insecure_mirrors()
            && (mirror.remote_url.starts_with("http://")
                || mirror.remote_url.starts_with("file://"));
        if !(mirror.remote_url.starts_with("https://")
            || mirror.remote_url.starts_with("ssh://")
            || mirror.remote_url.starts_with("git@")
            || insecure)
        {
            return Err(ForgeError::invalid(
                "bad_url",
                "mirror remote must be https://, ssh:// or git@",
            ));
        }
        if mirror.direction == 0 {
            return Err(ForgeError::missing("direction"));
        }
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        let m = RepoMirror {
            id: tx.new_id()?,
            repo_id: repo_id.into(),
            interval_seconds: if mirror.interval_seconds == 0 {
                3600
            } else {
                mirror.interval_seconds.max(300)
            },
            ..mirror
        };
        tx.insert(
            &MIRRORS,
            &Row::new(repo_id, m.id.clone()).msg(&MirrorRecord {
                mirror: Some(m.clone()),
                credential: credential.into(),
            }),
        )
        .await?;
        let after = serde_json::to_string(&m).unwrap_or_default();
        Self::repo_payload_event(&mut tx, "mirror.created", &repo, after.clone()).await?;
        tx.audit(
            AuditDraft::new("mirror.create", &repo.org_id)
                .repo_id(repo_id)
                .target_id(&m.id)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(m)
    }

    /// A repo's mirrors.
    pub async fn mirrors(&self, cx: &Cx, repo_id: &str) -> Result<Vec<RepoMirror>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(tx
            .scan_msgs::<MirrorRecord>(&MIRRORS, &Scan::scope(repo_id, 100))
            .await?
            .into_iter()
            .filter_map(|(m, _)| m.mirror)
            .collect())
    }

    /// A mirror with its credential.
    pub async fn mirror_record(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<MirrorRecord> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        tx.get_msg::<MirrorRecord>(&MIRRORS, repo_id, id)
            .await?
            .map(|(m, _)| m)
            .ok_or_else(|| ForgeError::not_found(format!("mirror {id}")))
    }

    /// Every mirror of every repo (the sync worker's schedule).
    pub async fn all_mirrors(&self, cx: &Cx) -> Result<Vec<RepoMirror>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(tx
            .scan_msgs::<MirrorRecord>(
                &MIRRORS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .filter_map(|(m, _)| m.mirror)
            .collect())
    }

    /// Records the outcome of a mirror sync: `last_sync_at` and `last_error` (empty on
    /// success), with a `mirror.synced` event.
    pub async fn record_mirror_sync(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
        error: &str,
    ) -> Result<RepoMirror> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        let Some((mut rec, row)) = tx.get_msg::<MirrorRecord>(&MIRRORS, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("mirror {id}")));
        };
        let mut m = rec.mirror.clone().unwrap_or_default();
        m.last_sync_at = Some(ts(tx.now_ms()));
        m.last_error = error.to_string();
        rec.mirror = Some(m.clone());
        tx.update(&MIRRORS, &row.clone().msg(&rec)).await?;
        Self::repo_payload_event(
            &mut tx,
            "mirror.synced",
            &repo,
            serde_json::json!({"id": id, "error": error}).to_string(),
        )
        .await?;
        tx.commit().await?;
        Ok(m)
    }

    /// Removes a mirror.
    pub async fn delete_mirror(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        if !tx.delete(&MIRRORS, repo_id, id).await? {
            return Err(ForgeError::not_found(format!("mirror {id}")));
        }
        Self::repo_payload_event(
            &mut tx,
            "mirror.deleted",
            &repo,
            serde_json::json!({"id": id}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("mirror.delete", &repo.org_id)
                .repo_id(repo_id)
                .target_id(id),
        )
        .await?;
        tx.commit().await
    }

    /// Adds a deploy key (unique per repo by key material).
    pub async fn add_deploy_key(
        &self,
        cx: &Cx,
        repo_id: &str,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<DeployKey> {
        let parts: Vec<&str> = public_key.split_whitespace().collect();
        if parts.len() < 2
            || !parts[0].starts_with("ssh-")
                && !parts[0].starts_with("ecdsa-")
                && !parts[0].starts_with("sk-")
        {
            return Err(ForgeError::invalid(
                "invalid_key",
                "public_key must be an OpenSSH public key",
            ));
        }
        let fingerprint = {
            use sha2::Digest;
            hex(&sha2::Sha256::digest(parts[1].as_bytes()))
        };
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        if !tx
            .scan(
                &DEPLOY_KEYS,
                &Scan::scope(repo_id, 1).key(Key::K1(fingerprint.clone())),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists("deploy key"));
        }
        let id = tx.new_id()?;
        let k = DeployKey {
            id: id.clone(),
            title: title.into(),
            public_key: format!("{} {}", parts[0], parts[1]),
            read_only,
            principal_id: format!("deploy-key:{id}"),
            created_at: Some(ts(tx.now_ms())),
            last_used_at: None,
        };
        tx.insert(
            &DEPLOY_KEYS,
            &Row::new(repo_id, id.clone()).k1(fingerprint).msg(&k),
        )
        .await?;
        let after =
            serde_json::json!({"id": id, "title": title, "read_only": read_only}).to_string();
        Self::repo_payload_event(&mut tx, "deploy_key.created", &repo, after.clone()).await?;
        tx.audit(
            AuditDraft::new(audit_actions::DEPLOY_KEY_CREATE, &repo.org_id)
                .repo_id(repo_id)
                .target_id(&id)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(k)
    }

    /// A repo's deploy keys.
    pub async fn deploy_keys(&self, cx: &Cx, repo_id: &str) -> Result<Vec<DeployKey>> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(tx
            .scan_msgs::<DeployKey>(&DEPLOY_KEYS, &Scan::scope(repo_id, 1000))
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect())
    }

    /// Sets (or, with an empty URL, clears) a repo's social preview image URL once its bytes
    /// are stored (`RepoService.CreateSocialPreviewUpload` hands out the upload URL).
    pub async fn set_social_preview(&self, cx: &Cx, id: &str, url: &str) -> Result<Repo> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (mut repo, row) = Self::live_repo(&mut tx, id).await?;
        let before = repo_json(&repo);
        repo.social_preview_url = url.to_string();
        repo.version = tx.update(&REPOS, &row.clone().msg(&repo)).await?;
        repo.full_name = format!(
            "{}/{}",
            Self::org_name(&mut tx, &repo.org_id).await?,
            repo.name
        );
        Self::repo_event(&mut tx, "repo.updated", &repo).await?;
        tx.audit(
            AuditDraft::new("repo.social_preview_update", &repo.org_id)
                .repo_id(id)
                .target(SubjectKind::Repo, id)
                .before(&before)
                .after(&repo_json(&repo)),
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    /// The fingerprint deploy keys are stored under (hex SHA-256 of the key's base64 text),
    /// for an OpenSSH public key line.
    pub fn deploy_key_fingerprint(public_key: &str) -> Option<String> {
        use sha2::Digest;
        let b64 = public_key.split_whitespace().nth(1)?;
        Some(hex(&sha2::Sha256::digest(b64.as_bytes())))
    }

    /// Every deploy key with this key material (any repo), as `(repo id, key)`: the SSH
    /// transport authenticates a connection by key before it knows the repo.
    pub async fn deploy_keys_by_material(
        &self,
        cx: &Cx,
        public_key: &str,
    ) -> Result<Vec<(String, DeployKey)>> {
        let Some(fp) = Self::deploy_key_fingerprint(public_key) else {
            return Ok(Vec::new());
        };
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        Ok(tx
            .scan_msgs::<DeployKey>(
                &DEPLOY_KEYS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .filter(|(_, row)| row.k1.as_deref() == Some(fp.as_str()))
            .map(|(k, row)| (row.scope, k))
            .collect())
    }

    /// Removes a deploy key.
    pub async fn delete_deploy_key(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db().begin(Surface::ORGS, cx).await?;
        let (repo, _) = Self::live_repo(&mut tx, repo_id).await?;
        if !tx.delete(&DEPLOY_KEYS, repo_id, id).await? {
            return Err(ForgeError::not_found(format!("deploy key {id}")));
        }
        Self::repo_payload_event(
            &mut tx,
            "deploy_key.deleted",
            &repo,
            serde_json::json!({"id": id}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new(audit_actions::DEPLOY_KEY_DELETE, &repo.org_id)
                .repo_id(repo_id)
                .target_id(id),
        )
        .await?;
        tx.commit().await
    }
}
