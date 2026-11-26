//! The packages store: owners, packages, versions, tags, access grants, downloads and the
//! generic publish every registry protocol uses.
//!
//! One method is one transaction on the `packages` surface (forge-core §2.3). Blob store
//! calls happen outside transactions: content is written before the transaction that
//! references it, and unreferenced content is deleted after the transaction that dropped the
//! last reference commits. Permission checks are not made here; callers authorize first with
//! [`Packages::authorize`] (the service and the protocol handlers both do).

use std::sync::Arc;

use bee_proto::v1::{
    ObjectId, Package, PackageAccessGrant, PackageFile, PackageGranteeKind, PackageOwnerKind,
    PackagePlatform, PackageRole, PackageUsage, PackageVersion, PackageVersionProvenance,
    PackageVersionUsage, PackageVisibility, Principal, PrincipalKind, RepoRef, SubjectKind,
    repo_ref,
};
use serde_json::json;

use super::model::{Eco, FileRec, PackageRec, PlatformRec, TagRec, VersionRec};
use super::ports::{PackagePorts, ProvenanceClaim};
use super::{
    BLOBREFS, COUNTS, DAILY, FILES, GRANTS, LINKS, OWNER, PACKAGES, PROVENANCE, SURFACE, TAGS,
    VERSIONS, names,
};
use crate::auth::Authz;
use crate::community::common::{draft, emit, paged, principal_ref};
use crate::community::tz::civil;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::hooks::PrincipalDirectory;
use crate::ids::ts;
use crate::orgs::Orgs;
use crate::store::{Db, Key, Row, Scan, Tx};

/// Settings of the packages module.
#[derive(Clone, Debug)]
pub struct PackagesConfig {
    /// Base URL of the forge, for links and generated registry URLs.
    pub base_url: String,
    /// Days a deleted package or version stays restorable.
    pub restore_window_days: u32,
    /// Largest single file accepted by the registry protocols.
    pub max_file_bytes: u64,
    /// Most files in one version.
    pub max_files_per_version: usize,
    /// A public package (or the last version of one) with more downloads than this cannot be
    /// deleted (`INVALID_STATE`).
    pub public_delete_download_limit: u64,
    /// npm: versions can be unpublished within this many hours of publishing.
    pub npm_unpublish_window_hours: u64,
    /// npm: or while the version has fewer downloads than this.
    pub npm_unpublish_download_limit: u64,
    /// Lifetime of an upload session (OCI blob uploads, native publishes).
    pub upload_ttl_ms: u64,
    /// Cargo `config.json` advertises `auth-required`.
    pub cargo_auth_required: bool,
}

impl Default for PackagesConfig {
    fn default() -> PackagesConfig {
        PackagesConfig {
            base_url: "https://beehive.invalid".into(),
            restore_window_days: 30,
            max_file_bytes: 1 << 30,
            max_files_per_version: 10_000,
            public_delete_download_limit: 5_000,
            npm_unpublish_window_hours: 72,
            npm_unpublish_download_limit: 300,
            upload_ttl_ms: 24 * 3_600_000,
            cargo_auth_required: true,
        }
    }
}

/// The org or user that owns packages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    /// Org or principal id.
    pub id: String,
    /// Org or user.
    pub kind: PackageOwnerKind,
    /// Org name or user login.
    pub name: String,
}

impl Owner {
    /// The org id for audit and events (empty for users).
    pub fn org_id(&self) -> &str {
        if self.kind == PackageOwnerKind::Org {
            &self.id
        } else {
            ""
        }
    }
}

/// A file ready to be attached to a version (its content is already in the blob store).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewFile {
    /// Name within the version.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// `sha256:<hex>`.
    pub digest: String,
    /// Media type.
    pub media_type: String,
    /// Blob store key.
    pub blob_key: String,
    /// SHA-1 hex, when known.
    pub sha1: String,
    /// MD5 hex, when known.
    pub md5: String,
    /// SHA-512 base64, when known.
    pub sha512_b64: String,
}

/// What publishing does when the version exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnConflict {
    /// `ALREADY_EXISTS` (`version_exists`): versions are immutable.
    Reject,
    /// Return the existing version (same key means same content), applying tags.
    Idempotent,
    /// Add the files to the existing version; `overwrite` allows replacing a file of the same
    /// name (Maven snapshots), otherwise that is `ALREADY_EXISTS` (`file_exists`).
    Merge {
        /// Replace files that already exist.
        overwrite: bool,
    },
}

/// A version to publish.
#[derive(Clone, Debug)]
pub struct NewVersion {
    /// Version string (containers: the manifest digest).
    pub version: String,
    /// Manifest or primary file digest.
    pub digest: String,
    /// Files.
    pub files: Vec<NewFile>,
    /// Tags to point at the version (container tags, npm dist-tags).
    pub tags: Vec<String>,
    /// Ecosystem metadata JSON.
    pub metadata_json: String,
    /// Platforms of an image index.
    pub platforms: Vec<PlatformRec>,
    /// Containers: manifest media type.
    pub media_type: String,
    /// Containers: the manifest digest this one refers to.
    pub subject: String,
    /// Containers: artifact type for referrers filtering.
    pub artifact_type: String,
    /// Behaviour when the version exists.
    pub on_conflict: OnConflict,
    /// How it was built.
    pub provenance: Option<ProvenanceClaim>,
}

impl NewVersion {
    /// A version with no files, tags or metadata that rejects duplicates.
    pub fn new(version: &str) -> NewVersion {
        NewVersion {
            version: version.to_string(),
            digest: String::new(),
            files: Vec::new(),
            tags: Vec::new(),
            metadata_json: String::new(),
            platforms: Vec::new(),
            media_type: String::new(),
            subject: String::new(),
            artifact_type: String::new(),
            on_conflict: OnConflict::Reject,
            provenance: None,
        }
    }
}

/// The outcome of a publish.
#[derive(Clone, Debug)]
pub struct Published {
    /// The version as stored.
    pub version: VersionRec,
    /// Whether it was created by this call.
    pub created: bool,
    /// Blob keys whose last reference this call dropped (already removed from the store).
    pub dropped: Vec<String>,
}

/// Filters of [`Packages::list`].
#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    /// Owner id (scope); empty to list through `repo_id` only.
    pub owner_id: String,
    /// Only this ecosystem.
    pub eco: Option<Eco>,
    /// Only this visibility.
    pub visibility: Option<PackageVisibility>,
    /// Only packages linked to this repo.
    pub repo_id: String,
    /// Include packages inside the restore window.
    pub include_deleted: bool,
}

/// Where to upload one file of a native publish.
#[derive(Clone, Debug)]
pub struct UploadTarget {
    /// File name within the version.
    pub name: String,
    /// Signed upload URL.
    pub url: String,
    /// Headers the upload must carry.
    pub headers: Vec<(String, String)>,
}

/// Packages and their versions over the forge store.
pub struct Packages {
    pub(crate) db: Db,
    pub(crate) ports: PackagePorts,
    pub(crate) orgs: Arc<Orgs>,
    pub(crate) directory: Arc<dyn PrincipalDirectory>,
    pub(crate) authz: Authz,
    pub(crate) config: PackagesConfig,
}

pub(crate) fn pkg_row(p: &PackageRec, version: u64) -> Result<Row> {
    let key = names::name_key(p.eco()?, &p.name);
    let r = Row::new(&p.owner_id, &p.id).msg(p).version(version);
    Ok(if p.deleted_ms == 0 {
        r.k1(key)
    } else {
        r.k2(key).n1(p.deleted_ms as i64)
    })
}

pub(crate) fn ver_row(p: &PackageRec, v: &VersionRec, version: u64) -> Result<Row> {
    let eco = p.eco()?;
    let key = names::version_key(eco, &v.version);
    let mut r = Row::new(&v.package_id, &v.id).msg(v).version(version);
    if v.deleted_ms == 0 {
        r = r.k1(key);
        if !v.subject.is_empty() {
            r = r.k2(v.subject.clone());
        }
    } else {
        r = r.n1(v.deleted_ms as i64);
        r = r.k2(if eco == Eco::Container {
            format!("deleted:{}", v.subject)
        } else {
            format!("del:{key}")
        });
    }
    Ok(r)
}

fn valid_tag(eco: Eco, t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '+' | '@'))
        && (eco != Eco::Npm || names::SemVer::parse(t).is_none())
}

impl Packages {
    /// Packages over `db`.
    pub fn new(
        db: Db,
        ports: PackagePorts,
        orgs: Arc<Orgs>,
        directory: Arc<dyn PrincipalDirectory>,
        authz: Authz,
        config: PackagesConfig,
    ) -> Packages {
        Packages {
            db,
            ports,
            orgs,
            directory,
            authz,
            config,
        }
    }

    /// Settings.
    pub fn config(&self) -> &PackagesConfig {
        &self.config
    }

    /// The ports.
    pub fn ports(&self) -> &PackagePorts {
        &self.ports
    }

    /// The store handle (shared with Pages in tests).
    pub fn db(&self) -> &Db {
        &self.db
    }

    // ---------- owners ----------

    /// Resolves an org name or id, else a user login or id, to an owner.
    pub async fn owner(&self, cx: &Cx, name_or_id: &str) -> Result<Owner> {
        if name_or_id.is_empty() {
            return Err(ForgeError::missing("owner"));
        }
        match self.orgs.get_org(cx, name_or_id).await {
            Ok(o) => {
                return Ok(Owner {
                    id: o.id,
                    kind: PackageOwnerKind::Org,
                    name: o.name,
                });
            }
            Err(e) if e.reason != bee_proto::v1::ErrorReason::NotFound => return Err(e),
            Err(_) => {}
        }
        let p = match self.directory.by_login(name_or_id).await {
            Some(p) => Some(p),
            None => self.directory.get(name_or_id).await,
        };
        match p {
            Some(p) => {
                let (logins, _) = self.directory.identities(&p.id).await;
                Ok(Owner {
                    name: logins.first().cloned().unwrap_or_else(|| p.id.clone()),
                    id: p.id,
                    kind: PackageOwnerKind::User,
                })
            }
            None => Err(ForgeError::not_found(format!("owner {name_or_id}"))),
        }
    }

    /// The owner of a stored package.
    pub async fn owner_of(&self, cx: &Cx, p: &PackageRec) -> Result<Owner> {
        match p.kind() {
            PackageOwnerKind::Org => {
                let o = self.orgs.org_any(cx, &p.owner_id).await?;
                Ok(Owner {
                    id: o.id,
                    kind: PackageOwnerKind::Org,
                    name: o.name,
                })
            }
            _ => {
                let (logins, _) = self.directory.identities(&p.owner_id).await;
                Ok(Owner {
                    id: p.owner_id.clone(),
                    kind: PackageOwnerKind::User,
                    name: logins
                        .first()
                        .cloned()
                        .unwrap_or_else(|| p.owner_id.clone()),
                })
            }
        }
    }

    // ---------- packages ----------

    pub(crate) async fn load_pkg(
        tx: &mut Tx,
        owner_id: &str,
        id: &str,
    ) -> Result<Option<(PackageRec, Row)>> {
        match tx.get(&PACKAGES, owner_id, id).await? {
            Some(r) => Ok(Some((r.decode()?, r))),
            None => Ok(None),
        }
    }

    pub(crate) async fn find_in(
        tx: &mut Tx,
        owner_id: &str,
        eco: Eco,
        name: &str,
        deleted: bool,
    ) -> Result<Option<(PackageRec, Row)>> {
        let key = names::name_key(eco, name);
        let k = if deleted { Key::K2(key) } else { Key::K1(key) };
        let rows = tx
            .scan(&PACKAGES, &Scan::scope(owner_id, 100).key(k))
            .await?;
        let mut best: Option<(PackageRec, Row)> = None;
        for r in rows {
            let p: PackageRec = r.decode()?;
            if best
                .as_ref()
                .is_none_or(|(b, _)| p.deleted_ms >= b.deleted_ms)
            {
                best = Some((p, r));
            }
        }
        Ok(best)
    }

    /// A live package by owner, ecosystem and name.
    pub async fn find(
        &self,
        cx: &Cx,
        owner: &Owner,
        eco: Eco,
        name: &str,
    ) -> Result<Option<PackageRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::find_in(&mut tx, &owner.id, eco, name, false)
            .await?
            .map(|(p, _)| p))
    }

    /// The most recently deleted package of that name inside the restore window.
    pub async fn find_deleted(
        &self,
        cx: &Cx,
        owner: &Owner,
        eco: Eco,
        name: &str,
    ) -> Result<Option<PackageRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::find_in(&mut tx, &owner.id, eco, name, true)
            .await?
            .map(|(p, _)| p))
    }

    /// A live package; `NOT_FOUND` when absent.
    pub async fn get(&self, cx: &Cx, owner: &Owner, eco: Eco, name: &str) -> Result<PackageRec> {
        self.find(cx, owner, eco, name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("package {}/{name}", owner.name)))
    }

    /// A package by id, including one inside the restore window.
    pub async fn get_by_id(&self, cx: &Cx, owner_id: &str, id: &str) -> Result<PackageRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load_pkg(&mut tx, owner_id, id)
            .await?
            .map(|(p, _)| p)
            .ok_or_else(|| ForgeError::not_found(format!("package {id}")))
    }

    /// One page of packages.
    pub async fn list(
        &self,
        cx: &Cx,
        f: &ListFilter,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<PackageRec>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if f.owner_id.is_empty() {
            if f.repo_id.is_empty() {
                return Err(ForgeError::missing("owner"));
            }
            let (rows, next) = paged(
                &mut tx,
                &LINKS,
                Scan::scope(&f.repo_id, limit),
                token,
                limit,
            )
            .await?;
            let mut out = Vec::new();
            for l in rows {
                let owner = l.k1.clone().unwrap_or_default();
                if let Some((p, _)) = Self::load_pkg(&mut tx, &owner, &l.id).await?
                    && self.listed(&p, f)
                {
                    out.push(p);
                }
            }
            return Ok((out, next));
        }
        let (rows, next) = paged(
            &mut tx,
            &PACKAGES,
            Scan::scope(&f.owner_id, limit),
            token,
            limit,
        )
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let p: PackageRec = r.decode()?;
            if self.listed(&p, f) && (f.repo_id.is_empty() || p.repo_id == f.repo_id) {
                out.push(p);
            }
        }
        Ok((out, next))
    }

    fn listed(&self, p: &PackageRec, f: &ListFilter) -> bool {
        (f.include_deleted || p.deleted_ms == 0)
            && f.eco.is_none_or(|e| e.number() == p.ecosystem)
            && f.visibility.is_none_or(|v| v as i32 == p.visibility)
    }

    fn pkg_event(p: &PackageRec, owner: &Owner, extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({
            "package": p.name, "ecosystem": p.ecosystem, "owner": owner.name,
            "visibility": p.visibility, "repo_id": p.repo_id,
        });
        if let (Some(a), Some(b)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in b {
                a.insert(k.clone(), x.clone());
            }
        }
        v
    }

    async fn announce(
        tx: &mut Tx,
        kind: &str,
        audit: &str,
        owner: &Owner,
        p: &PackageRec,
        extra: serde_json::Value,
    ) -> Result<()> {
        let payload = Self::pkg_event(p, owner, extra);
        let subject = bee_proto::v1::SubjectRef {
            kind: SubjectKind::Package as i32,
            repo: crate::community::common::repo_ref_id(&p.repo_id),
            id: p.id.clone(),
        };
        emit(
            tx,
            kind,
            owner.org_id(),
            &p.repo_id,
            subject,
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            audit,
            owner.org_id(),
            &p.repo_id,
            SubjectKind::Package,
            &p.id,
            &payload,
        ))
        .await
    }

    /// Creates a package, or returns the live one of that name. The bool says it was created.
    /// `repo_id` links it on creation. Public visibility is refused when the owner forbids it.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_if_missing(
        &self,
        cx: &Cx,
        owner: &Owner,
        eco: Eco,
        name: &str,
        visibility: PackageVisibility,
        repo_id: &str,
        description: &str,
    ) -> Result<(PackageRec, bool)> {
        names::validate_name(eco, name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some((p, _)) = Self::find_in(&mut tx, &owner.id, eco, name, false).await? {
            return Ok((p, false));
        }
        if visibility == PackageVisibility::Public {
            let s = Self::owner_settings_in(&mut tx, &owner.id).await?;
            if s.forbid_public {
                return Err(ForgeError::policy_denied(
                    "packages.forbid_public",
                    vec!["visibility:public".into()],
                    format!("{} does not allow public packages", owner.name),
                ));
            }
        }
        let now = tx.now_ms();
        let p = PackageRec {
            id: tx.new_id()?,
            owner_id: owner.id.clone(),
            owner_kind: owner.kind as i32,
            ecosystem: eco.number(),
            name: name.to_string(),
            visibility: visibility as i32,
            repo_id: repo_id.to_string(),
            inherit_access: !repo_id.is_empty(),
            description: description.to_string(),
            created_ms: now,
            updated_ms: now,
            deleted_ms: 0,
            creator: cx.actor_id.clone(),
            version_count: 0,
        };
        tx.insert(&PACKAGES, &pkg_row(&p, 0)?).await?;
        if !repo_id.is_empty() {
            tx.insert(&LINKS, &Row::new(repo_id, &p.id).k1(&owner.id))
                .await?;
        }
        Self::announce(
            &mut tx,
            "package.created",
            "package.create",
            owner,
            &p,
            json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok((p, true))
    }

    #[allow(clippy::too_many_arguments)]
    async fn mutate_pkg<F>(
        &self,
        cx: &Cx,
        owner: &Owner,
        id: &str,
        f: F,
        kind: &str,
        audit: &str,
        links: Option<(&str, &str)>,
    ) -> Result<PackageRec>
    where
        F: FnOnce(&mut PackageRec, &mut Tx) -> Result<serde_json::Value>,
    {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut p, row)) = Self::load_pkg(&mut tx, &owner.id, id).await? else {
            return Err(ForgeError::not_found(format!("package {id}")));
        };
        if p.deleted_ms != 0 {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        }
        let before = Self::pkg_event(&p, owner, json!({}));
        let extra = f(&mut p, &mut tx)?;
        p.updated_ms = tx.now_ms();
        tx.update(&PACKAGES, &pkg_row(&p, row.version)?).await?;
        if let Some((old, new)) = links {
            if !old.is_empty() {
                tx.delete(&LINKS, old, &p.id).await?;
            }
            if !new.is_empty() {
                tx.put(&LINKS, &Row::new(new, &p.id).k1(&owner.id)).await?;
            }
        }
        let mut payload = Self::pkg_event(&p, owner, extra);
        if let Some(o) = payload.as_object_mut() {
            o.insert("before".into(), before);
        }
        let subject = bee_proto::v1::SubjectRef {
            kind: SubjectKind::Package as i32,
            repo: crate::community::common::repo_ref_id(&p.repo_id),
            id: p.id.clone(),
        };
        emit(
            &mut tx,
            kind,
            owner.org_id(),
            &p.repo_id,
            subject,
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            audit,
            owner.org_id(),
            &p.repo_id,
            SubjectKind::Package,
            &p.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Updates the description and access inheritance.
    pub async fn update(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        description: Option<String>,
        inherit: Option<bool>,
    ) -> Result<PackageRec> {
        if let Some(d) = &description
            && d.len() > 4096
        {
            return Err(ForgeError::too_large("description", 4096, d.len() as u64));
        }
        self.mutate_pkg(
            cx,
            owner,
            &p.id,
            |p, _| {
                if let Some(d) = description {
                    p.description = d;
                }
                if let Some(i) = inherit {
                    if i && p.repo_id.is_empty() {
                        return Err(ForgeError::invalid_state(
                            "the package is not linked to a repo",
                        ));
                    }
                    p.inherit_access = i;
                }
                Ok(json!({}))
            },
            "package.updated",
            "package.update",
            None,
        )
        .await
    }

    /// Changes the visibility; `POLICY_DENIED` when making it public under an owner that
    /// forbids public packages.
    pub async fn set_visibility(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        v: PackageVisibility,
    ) -> Result<PackageRec> {
        if v == PackageVisibility::Unspecified {
            return Err(ForgeError::missing("visibility"));
        }
        if v == PackageVisibility::Public {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if Self::owner_settings_in(&mut tx, &owner.id)
                .await?
                .forbid_public
            {
                return Err(ForgeError::policy_denied(
                    "packages.forbid_public",
                    vec!["visibility:public".into()],
                    format!("{} does not allow public packages", owner.name),
                ));
            }
        }
        if v == PackageVisibility::Internal && owner.kind != PackageOwnerKind::Org {
            return Err(ForgeError::invalid(
                "bad_visibility",
                "internal visibility needs an org owner",
            ));
        }
        self.mutate_pkg(
            cx,
            owner,
            &p.id,
            |p, _| {
                p.visibility = v as i32;
                Ok(json!({"new_visibility": v as i32}))
            },
            "package.visibility_changed",
            "package.visibility.set",
            None,
        )
        .await
    }

    /// Links a package to a repo (replacing any link) and turns on access inheritance.
    pub async fn link_repo(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        repo_id: &str,
    ) -> Result<PackageRec> {
        let old = p.repo_id.clone();
        let new = repo_id.to_string();
        let owner_id = owner.id.clone();
        self.mutate_pkg(
            cx,
            owner,
            &p.id,
            |p, _| {
                let previous = std::mem::replace(&mut p.repo_id, new.clone());
                p.inherit_access = true;
                Ok(json!({"linked_repo_id": new, "previous_repo_id": previous, "owner_id": owner_id}))
            },
            "package.linked",
            "package.link",
            Some((&old, repo_id)),
        )
        .await
    }

    /// Removes the repo link (and access inheritance with it).
    pub async fn unlink_repo(&self, cx: &Cx, owner: &Owner, p: &PackageRec) -> Result<PackageRec> {
        let old = p.repo_id.clone();
        self.mutate_pkg(
            cx,
            owner,
            &p.id,
            |p, _| {
                p.repo_id.clear();
                p.inherit_access = false;
                Ok(json!({"previous_repo_id": old}))
            },
            "package.unlinked",
            "package.unlink",
            Some((&old, "")),
        )
        .await
    }

    /// Deletes a package and all its versions (restorable inside the window). A public package
    /// with more downloads than [`PackagesConfig::public_delete_download_limit`] is
    /// `INVALID_STATE`.
    pub async fn delete(&self, cx: &Cx, owner: &Owner, p: &PackageRec) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut cur, row)) = Self::load_pkg(&mut tx, &owner.id, &p.id).await? else {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        };
        if cur.deleted_ms != 0 {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        }
        if cur.vis() == PackageVisibility::Public {
            let downloads = Self::counter(&mut tx, &cur.id, "").await?;
            if downloads > self.config.public_delete_download_limit {
                return Err(ForgeError::invalid_state(format!(
                    "a public package with more than {} downloads cannot be deleted",
                    self.config.public_delete_download_limit
                )));
            }
        }
        cur.deleted_ms = tx.now_ms();
        cur.updated_ms = cur.deleted_ms;
        tx.update(&PACKAGES, &pkg_row(&cur, row.version)?).await?;
        if !cur.repo_id.is_empty() {
            tx.delete(&LINKS, &cur.repo_id, &cur.id).await?;
        }
        Self::announce(
            &mut tx,
            "package.deleted",
            "package.delete",
            owner,
            &cur,
            json!({}),
        )
        .await?;
        tx.commit().await
    }

    /// Restores a deleted package inside the window. `NOT_FOUND` past the window or when never
    /// deleted, `ALREADY_EXISTS` when the name is taken again.
    pub async fn restore(
        &self,
        cx: &Cx,
        owner: &Owner,
        eco: Eco,
        name: &str,
    ) -> Result<PackageRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut cur, row)) = Self::find_in(&mut tx, &owner.id, eco, name, true).await? else {
            return Err(ForgeError::not_found(format!("deleted package {name}")));
        };
        let window = u64::from(self.config.restore_window_days) * 86_400_000;
        if cur.deleted_ms + window <= now {
            return Err(ForgeError::not_found(format!("deleted package {name}")));
        }
        if Self::find_in(&mut tx, &owner.id, eco, name, false)
            .await?
            .is_some()
        {
            return Err(ForgeError::already_exists(format!("package {name}")));
        }
        cur.deleted_ms = 0;
        cur.updated_ms = now;
        tx.update(&PACKAGES, &pkg_row(&cur, row.version)?).await?;
        if !cur.repo_id.is_empty() {
            tx.put(&LINKS, &Row::new(&cur.repo_id, &cur.id).k1(&owner.id))
                .await?;
        }
        Self::announce(
            &mut tx,
            "package.restored",
            "package.restore",
            owner,
            &cur,
            json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(cur)
    }

    // ---------- grants ----------

    fn grant_id(kind: PackageGranteeKind, ident: &str) -> String {
        format!("{}:{ident}", kind as i32)
    }

    /// Explicit grants of a package, by id.
    pub async fn grants(
        &self,
        cx: &Cx,
        p: &PackageRec,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<PackageAccessGrant>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = paged(&mut tx, &GRANTS, Scan::scope(&p.id, limit), token, limit).await?;
        let out = rows
            .iter()
            .map(|r| r.decode::<PackageAccessGrant>())
            .collect::<Result<Vec<_>>>()?;
        Ok((out, next))
    }

    pub(crate) async fn grants_all(
        &self,
        cx: &Cx,
        p: &PackageRec,
    ) -> Result<Vec<PackageAccessGrant>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx.scan(&GRANTS, &Scan::scope(&p.id, 1000)).await?;
        rows.iter().map(|r| r.decode()).collect()
    }

    /// Grants (or changes) a role.
    #[allow(clippy::too_many_arguments)]
    pub async fn grant(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        kind: PackageGranteeKind,
        ident: &str,
        repo_id: &str,
        role: PackageRole,
    ) -> Result<PackageAccessGrant> {
        if role == PackageRole::Unspecified {
            return Err(ForgeError::missing("role"));
        }
        if ident.is_empty() {
            return Err(ForgeError::missing("grantee"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if Self::load_pkg(&mut tx, &owner.id, &p.id)
            .await?
            .is_none_or(|(c, _)| c.deleted_ms != 0)
        {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        }
        let now = tx.now_ms();
        let id = Self::grant_id(kind, ident);
        let g = PackageAccessGrant {
            kind: kind as i32,
            repo: if kind == PackageGranteeKind::Repo {
                Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo_id.to_string())),
                })
            } else {
                None
            },
            team_id: if kind == PackageGranteeKind::Team {
                ident.to_string()
            } else {
                String::new()
            },
            principal: if kind == PackageGranteeKind::Principal {
                Some(principal_ref(ident))
            } else {
                None
            },
            role: role as i32,
            created_at: Some(ts(now)),
        };
        tx.put(&GRANTS, &Row::new(&p.id, &id).msg(&g)).await?;
        let payload = Self::pkg_event(
            p,
            owner,
            json!({"grantee": ident, "kind": kind as i32, "role": role as i32}),
        );
        let subject = bee_proto::v1::SubjectRef {
            kind: SubjectKind::Package as i32,
            repo: crate::community::common::repo_ref_id(&p.repo_id),
            id: p.id.clone(),
        };
        emit(
            &mut tx,
            "package.access_granted",
            owner.org_id(),
            &p.repo_id,
            subject,
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "package.access.grant",
            owner.org_id(),
            &p.repo_id,
            SubjectKind::Package,
            &p.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(g)
    }

    /// Removes a grant; `NOT_FOUND` when it does not exist.
    pub async fn revoke(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        kind: PackageGranteeKind,
        ident: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = Self::grant_id(kind, ident);
        if !tx.delete(&GRANTS, &p.id, &id).await? {
            return Err(ForgeError::not_found("access grant"));
        }
        let payload = Self::pkg_event(p, owner, json!({"grantee": ident, "kind": kind as i32}));
        let subject = bee_proto::v1::SubjectRef {
            kind: SubjectKind::Package as i32,
            repo: crate::community::common::repo_ref_id(&p.repo_id),
            id: p.id.clone(),
        };
        emit(
            &mut tx,
            "package.access_revoked",
            owner.org_id(),
            &p.repo_id,
            subject,
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "package.access.revoke",
            owner.org_id(),
            &p.repo_id,
            SubjectKind::Package,
            &p.id,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    // ---------- versions ----------

    pub(crate) async fn find_version(
        tx: &mut Tx,
        pkg_id: &str,
        key: &str,
    ) -> Result<Option<(VersionRec, Row)>> {
        let rows = tx
            .scan(
                &VERSIONS,
                &Scan::scope(pkg_id, 2).key(Key::K1(key.to_string())),
            )
            .await?;
        match rows.into_iter().next() {
            Some(r) => Ok(Some((r.decode()?, r))),
            None => Ok(None),
        }
    }

    pub(crate) async fn load_version(
        tx: &mut Tx,
        pkg_id: &str,
        id: &str,
    ) -> Result<Option<(VersionRec, Row)>> {
        match tx.get(&VERSIONS, pkg_id, id).await? {
            Some(r) => Ok(Some((r.decode()?, r))),
            None => Ok(None),
        }
    }

    /// Finds a live version by id, tag or version key.
    pub(crate) async fn resolve_in(
        tx: &mut Tx,
        p: &PackageRec,
        reference: &str,
    ) -> Result<Option<(VersionRec, Row)>> {
        let eco = p.eco()?;
        if reference.len() == 26
            && let Some((v, r)) = Self::load_version(tx, &p.id, reference).await?
            && v.deleted_ms == 0
        {
            return Ok(Some((v, r)));
        }
        if let Some(t) = tx.get(&TAGS, &p.id, reference).await? {
            let tag: TagRec = t.decode()?;
            if let Some((v, r)) = Self::load_version(tx, &p.id, &tag.version_id).await?
                && v.deleted_ms == 0
            {
                return Ok(Some((v, r)));
            }
        }
        let key = names::version_key(eco, reference);
        Self::find_version(tx, &p.id, &key).await
    }

    /// A live version by id, tag, version string or digest.
    pub async fn version(&self, cx: &Cx, p: &PackageRec, reference: &str) -> Result<VersionRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::resolve_in(&mut tx, p, reference)
            .await?
            .map(|(v, _)| v)
            .ok_or_else(|| ForgeError::not_found(format!("version {reference} of {}", p.name)))
    }

    /// A version by id, deleted ones included.
    pub async fn version_any(&self, cx: &Cx, p: &PackageRec, id: &str) -> Result<VersionRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load_version(&mut tx, &p.id, id)
            .await?
            .map(|(v, _)| v)
            .ok_or_else(|| ForgeError::not_found(format!("version {id}")))
    }

    /// Live versions, newest first.
    pub async fn versions(
        &self,
        cx: &Cx,
        p: &PackageRec,
        include_deleted: bool,
        tagged_only: bool,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<VersionRec>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let tagged: std::collections::BTreeSet<String> = if tagged_only {
            tx.scan(&TAGS, &Scan::scope(&p.id, 10_000))
                .await?
                .into_iter()
                .filter_map(|r| r.k1)
                .collect()
        } else {
            Default::default()
        };
        let mut out = Vec::new();
        let mut cursor = token.to_string();
        loop {
            let (rows, next) = paged(
                &mut tx,
                &VERSIONS,
                Scan::scope(&p.id, limit).desc(),
                &cursor,
                limit,
            )
            .await?;
            for r in &rows {
                let v: VersionRec = r.decode()?;
                if (include_deleted || v.deleted_ms == 0)
                    && (!tagged_only || tagged.contains(&v.id))
                {
                    out.push(v);
                }
            }
            if out.len() >= limit || next.is_empty() {
                let more = out.len() > limit;
                out.truncate(limit);
                // Without a filter a short page means the end; with one, resume after the page.
                return Ok((
                    out,
                    if more || !next.is_empty() {
                        next
                    } else {
                        String::new()
                    },
                ));
            }
            cursor = next;
        }
    }

    /// Every live version, newest first (protocol listings).
    pub async fn all_versions(&self, cx: &Cx, p: &PackageRec) -> Result<Vec<VersionRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(&VERSIONS, &Scan::scope(&p.id, 10_000).desc())
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let v: VersionRec = r.decode()?;
            if v.deleted_ms == 0 {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// The tags of a package: tag name to version id.
    pub async fn tags(&self, cx: &Cx, p: &PackageRec) -> Result<Vec<(String, String)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx.scan(&TAGS, &Scan::scope(&p.id, 10_000)).await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.id, r.k1.unwrap_or_default()))
            .collect())
    }

    /// Tags of one version.
    pub async fn version_tags(&self, cx: &Cx, p: &PackageRec, vid: &str) -> Result<Vec<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::tags_in(&mut tx, &p.id, vid).await
    }

    async fn tags_in(tx: &mut Tx, pkg_id: &str, vid: &str) -> Result<Vec<String>> {
        let rows = tx
            .scan(
                &TAGS,
                &Scan::scope(pkg_id, 1000).key(Key::K1(vid.to_string())),
            )
            .await?;
        let mut v: Vec<String> = rows.into_iter().map(|r| r.id).collect();
        v.sort();
        Ok(v)
    }

    /// Files of a version, by name.
    pub async fn files(&self, cx: &Cx, v: &VersionRec) -> Result<Vec<FileRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx.scan(&FILES, &Scan::scope(&v.id, 10_000)).await?;
        rows.iter().map(|r| r.decode()).collect()
    }

    /// One file of a version.
    pub async fn file(&self, cx: &Cx, v: &VersionRec, name: &str) -> Result<Option<FileRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match tx.get(&FILES, &v.id, name).await? {
            Some(r) => Ok(Some(r.decode()?)),
            None => Ok(None),
        }
    }

    pub(crate) async fn counter(tx: &mut Tx, scope: &str, id: &str) -> Result<u64> {
        Ok(tx
            .get(&COUNTS, scope, id)
            .await?
            .and_then(|r| r.n1)
            .unwrap_or(0)
            .max(0) as u64)
    }

    pub(crate) async fn put_file(
        tx: &mut Tx,
        owner_id: &str,
        vid: &str,
        f: &NewFile,
        now: u64,
        overwrite: bool,
        dropped: &mut Vec<String>,
    ) -> Result<i64> {
        let old = tx.get(&FILES, vid, &f.name).await?;
        let mut delta = f.size as i64;
        if let Some(o) = &old {
            if !overwrite {
                return Err(ForgeError::already_exists(format!("file {}", f.name))
                    .with_subcode("file_exists"));
            }
            let o: FileRec = o.decode()?;
            delta -= o.size as i64;
            if o.blob_key != f.blob_key
                && tx.add_n1(&BLOBREFS, owner_id, &o.blob_key, -1).await? <= 0
            {
                dropped.push(o.blob_key);
            }
        }
        if old.as_ref().is_none_or(|o| {
            o.decode::<FileRec>()
                .map(|o| o.blob_key != f.blob_key)
                .unwrap_or(true)
        }) {
            tx.add_n1(&BLOBREFS, owner_id, &f.blob_key, 1).await?;
        }
        let rec = FileRec {
            name: f.name.clone(),
            size: f.size,
            digest: f.digest.clone(),
            media_type: f.media_type.clone(),
            blob_key: f.blob_key.clone(),
            sha1: f.sha1.clone(),
            md5: f.md5.clone(),
            sha512_b64: f.sha512_b64.clone(),
            created_ms: now,
        };
        tx.put(&FILES, &Row::new(vid, &f.name).msg(&rec)).await?;
        Ok(delta)
    }

    pub(crate) async fn put_tags(
        tx: &mut Tx,
        eco: Eco,
        pkg_id: &str,
        vid: &str,
        tags: &[String],
        now: u64,
    ) -> Result<()> {
        for t in tags {
            if !valid_tag(eco, t) {
                return Err(ForgeError::invalid(
                    "bad_tag",
                    format!("`{t}` is not a valid tag"),
                ));
            }
        }
        for t in tags {
            let rec = TagRec {
                tag: t.clone(),
                version_id: vid.to_string(),
                updated_ms: now,
            };
            tx.put(&TAGS, &Row::new(pkg_id, t).k1(vid).msg(&rec))
                .await?;
        }
        Ok(())
    }

    /// Publishes a version whose files are already in the blob store.
    pub async fn publish(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        nv: NewVersion,
    ) -> Result<Published> {
        let eco = p.eco()?;
        names::validate_version(eco, &nv.version)?;
        if nv.files.len() > self.config.max_files_per_version {
            return Err(ForgeError::too_large(
                "files per version",
                self.config.max_files_per_version as u64,
                nv.files.len() as u64,
            ));
        }
        for f in &nv.files {
            if f.size > self.config.max_file_bytes {
                return Err(ForgeError::too_large(
                    "file size",
                    self.config.max_file_bytes,
                    f.size,
                ));
            }
        }
        let key = names::version_key(eco, &nv.version);
        let verified = match &nv.provenance {
            Some(c) => self.ports.provenance.verify(cx, c).await.unwrap_or(false),
            None => false,
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        // Serialize storage quota decisions for this owner across Postgres replicas.
        tx.add_n1(&COUNTS, &owner.id, "storage", 0).await?;
        let now = tx.now_ms();
        let Some((mut cur, prow)) = Self::load_pkg(&mut tx, &owner.id, &p.id).await? else {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        };
        if cur.deleted_ms != 0 {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        }
        let added: u64 = nv.files.iter().map(|f| f.size).sum();
        let settings = Self::owner_settings_in(&mut tx, &owner.id).await?;
        let used = Self::counter(&mut tx, &owner.id, "storage").await?;
        let mut dropped = Vec::new();
        let existing = Self::find_version(&mut tx, &cur.id, &key).await?;
        let (mut v, vrow, created) = match existing {
            Some((v, row)) => match nv.on_conflict {
                OnConflict::Reject => {
                    return Err(ForgeError::already_exists(format!(
                        "version {} of {}",
                        nv.version, cur.name
                    ))
                    .with_subcode("version_exists"));
                }
                OnConflict::Idempotent | OnConflict::Merge { .. } => (v, Some(row), false),
            },
            None => {
                if eco != Eco::Container
                    && !tx
                        .scan(
                            &VERSIONS,
                            &Scan::scope(&cur.id, 1).key(Key::K2(format!("del:{key}"))),
                        )
                        .await?
                        .is_empty()
                {
                    return Err(ForgeError::already_exists(format!(
                        "version {} of {}",
                        nv.version, cur.name
                    ))
                    .with_subcode("version_deleted"));
                }
                let v = VersionRec {
                    id: tx.new_id()?,
                    package_id: cur.id.clone(),
                    version: nv.version.clone(),
                    digest: nv.digest.clone(),
                    size_bytes: 0,
                    publisher_id: cx.actor_id.clone(),
                    publisher_kind: cx.actor_kind as i32,
                    changeset: nv
                        .provenance
                        .as_ref()
                        .map(|c| c.changeset.clone())
                        .unwrap_or_default(),
                    run_id: nv
                        .provenance
                        .as_ref()
                        .map(|c| c.run_id.clone())
                        .unwrap_or_default(),
                    has_provenance: nv.provenance.is_some(),
                    created_ms: now,
                    updated_ms: now,
                    deleted_ms: 0,
                    metadata_json: nv.metadata_json.clone(),
                    platforms: nv.platforms.clone(),
                    deprecated: String::new(),
                    yanked: false,
                    media_type: nv.media_type.clone(),
                    subject: nv.subject.clone(),
                    artifact_type: nv.artifact_type.clone(),
                    owner_id: owner.id.clone(),
                };
                (v, None, true)
            }
        };
        let merge_overwrite = matches!(nv.on_conflict, OnConflict::Merge { overwrite: true });
        let mut delta: i64 = 0;
        if created || matches!(nv.on_conflict, OnConflict::Merge { .. }) {
            if settings.storage_quota_bytes > 0 && used + added > settings.storage_quota_bytes {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::QuotaExceeded,
                    format!(
                        "{} would exceed its package storage quota of {} bytes",
                        owner.name, settings.storage_quota_bytes
                    ),
                ));
            }
            for f in &nv.files {
                delta += Self::put_file(
                    &mut tx,
                    &owner.id,
                    &v.id,
                    f,
                    now,
                    merge_overwrite || created,
                    &mut dropped,
                )
                .await?;
            }
        }
        if delta != 0 {
            tx.add_n1(&COUNTS, &owner.id, "storage", delta).await?;
        }
        v.size_bytes = (v.size_bytes as i64 + delta).max(0) as u64;
        v.updated_ms = now;
        if created {
            tx.insert(&VERSIONS, &ver_row(&cur, &v, 0)?).await?;
            cur.version_count += 1;
            cur.updated_ms = now;
            tx.update(&PACKAGES, &pkg_row(&cur, prow.version)?).await?;
        } else if let Some(row) = vrow
            && (delta != 0 || matches!(nv.on_conflict, OnConflict::Merge { .. }))
        {
            tx.update(&VERSIONS, &ver_row(&cur, &v, row.version)?)
                .await?;
        }
        let mut moved = false;
        if !nv.tags.is_empty() {
            let before = Self::tags_in(&mut tx, &cur.id, &v.id).await?;
            Self::put_tags(&mut tx, eco, &cur.id, &v.id, &nv.tags, now).await?;
            moved = nv.tags.iter().any(|t| !before.contains(t));
        }
        if created && let Some(c) = &nv.provenance {
            let pr = PackageVersionProvenance {
                version_id: v.id.clone(),
                source_repo: crate::community::common::repo_ref_id(&c.repo_id),
                source_ref: c.git_ref.clone(),
                source_changeset: Some(ObjectId {
                    raw: c.changeset.clone(),
                }),
                workflow_run_id: c.run_id.clone(),
                workflow_path: c.workflow_path.clone(),
                job_id: c.job_id.clone(),
                signer_identity: c.signer_identity.clone(),
                issuer: c.issuer.clone(),
                attestation_ids: c.attestation_ids.clone(),
                provenance: None,
                verified,
            };
            tx.put(&PROVENANCE, &Row::new(&cur.id, &v.id).msg(&pr))
                .await?;
        }
        if created {
            Self::announce(
                &mut tx,
                "package.published",
                "package.publish",
                owner,
                &cur,
                json!({"version": v.version, "version_id": v.id, "digest": v.digest, "size_bytes": v.size_bytes}),
            )
            .await?;
        } else if moved {
            Self::announce(
                &mut tx,
                "package.tags_updated",
                "package.tags.set",
                owner,
                &cur,
                json!({"version": v.version, "version_id": v.id, "tags": nv.tags}),
            )
            .await?;
        }
        tx.commit().await?;
        self.drop_blobs(&dropped).await;
        Ok(Published {
            version: v,
            created,
            dropped,
        })
    }

    pub(crate) async fn drop_blobs(&self, keys: &[String]) {
        for k in keys {
            let _ = self.ports.blobs.delete(k).await;
        }
    }

    /// Deletes a version (restorable inside the window), and its tags.
    pub async fn delete_version(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        reference: &str,
    ) -> Result<VersionRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut cur, prow)) = Self::load_pkg(&mut tx, &owner.id, &p.id).await? else {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        };
        let Some((mut v, row)) = Self::resolve_in(&mut tx, &cur, reference).await? else {
            return Err(ForgeError::not_found(format!("version {reference}")));
        };
        if cur.vis() == PackageVisibility::Public {
            let downloads = Self::counter(&mut tx, &cur.id, &v.id).await?;
            if cur.version_count <= 1 && downloads > self.config.public_delete_download_limit {
                return Err(ForgeError::invalid_state(format!(
                    "the last version of a public package with more than {} downloads cannot be deleted",
                    self.config.public_delete_download_limit
                )));
            }
        }
        v.deleted_ms = now;
        v.updated_ms = now;
        tx.update(&VERSIONS, &ver_row(&cur, &v, row.version)?)
            .await?;
        for t in tx
            .scan(
                &TAGS,
                &Scan::scope(&cur.id, 1000).key(Key::K1(v.id.clone())),
            )
            .await?
        {
            tx.delete(&TAGS, &cur.id, &t.id).await?;
        }
        cur.version_count = cur.version_count.saturating_sub(1);
        cur.updated_ms = now;
        tx.update(&PACKAGES, &pkg_row(&cur, prow.version)?).await?;
        Self::announce(
            &mut tx,
            "package.version_deleted",
            "package.version.delete",
            owner,
            &cur,
            json!({"version": v.version, "version_id": v.id}),
        )
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// Restores a deleted version inside the window.
    pub async fn restore_version(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        version_id: &str,
    ) -> Result<VersionRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut cur, prow)) = Self::load_pkg(&mut tx, &owner.id, &p.id).await? else {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        };
        let Some((mut v, row)) = Self::load_version(&mut tx, &cur.id, version_id).await? else {
            return Err(ForgeError::not_found(format!("version {version_id}")));
        };
        let window = u64::from(self.config.restore_window_days) * 86_400_000;
        if v.deleted_ms == 0 || v.deleted_ms + window <= now {
            return Err(ForgeError::not_found(format!(
                "deleted version {version_id}"
            )));
        }
        let key = names::version_key(cur.eco()?, &v.version);
        if Self::find_version(&mut tx, &cur.id, &key).await?.is_some() {
            return Err(ForgeError::already_exists(format!("version {}", v.version)));
        }
        v.deleted_ms = 0;
        v.updated_ms = now;
        tx.update(&VERSIONS, &ver_row(&cur, &v, row.version)?)
            .await?;
        cur.version_count += 1;
        cur.updated_ms = now;
        tx.update(&PACKAGES, &pkg_row(&cur, prow.version)?).await?;
        Self::announce(
            &mut tx,
            "package.version_restored",
            "package.version.restore",
            owner,
            &cur,
            json!({"version": v.version, "version_id": v.id}),
        )
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// Replaces a version's tags and moves them off other versions.
    pub async fn set_tags(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        reference: &str,
        tags: &[String],
    ) -> Result<VersionRec> {
        let eco = p.eco()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((cur, _)) = Self::load_pkg(&mut tx, &owner.id, &p.id).await? else {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        };
        let Some((v, _)) = Self::resolve_in(&mut tx, &cur, reference).await? else {
            return Err(ForgeError::not_found(format!("version {reference}")));
        };
        let old = Self::tags_in(&mut tx, &cur.id, &v.id).await?;
        for t in &old {
            if !tags.contains(t) {
                tx.delete(&TAGS, &cur.id, t).await?;
            }
        }
        Self::put_tags(&mut tx, eco, &cur.id, &v.id, tags, now).await?;
        Self::announce(
            &mut tx,
            "package.tags_updated",
            "package.tags.set",
            owner,
            &cur,
            json!({"version": v.version, "version_id": v.id, "tags": tags}),
        )
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// Removes one tag (dist-tag removal, OCI tag deletion). `NOT_FOUND` when absent.
    pub async fn delete_tag(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        tag: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&TAGS, &p.id, tag).await? {
            return Err(ForgeError::not_found(format!("tag {tag}")));
        }
        Self::announce(
            &mut tx,
            "package.tags_updated",
            "package.tags.set",
            owner,
            p,
            json!({"removed_tag": tag}),
        )
        .await?;
        tx.commit().await
    }

    /// Sets the npm deprecation message (empty clears it) or the yank flag of a version.
    pub async fn update_version_flags(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        reference: &str,
        deprecated: Option<String>,
        yanked: Option<bool>,
    ) -> Result<VersionRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((cur, _)) = Self::load_pkg(&mut tx, &owner.id, &p.id).await? else {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        };
        let Some((mut v, row)) = Self::resolve_in(&mut tx, &cur, reference).await? else {
            return Err(ForgeError::not_found(format!("version {reference}")));
        };
        let changed = deprecated.as_ref().is_some_and(|d| *d != v.deprecated)
            || yanked.is_some_and(|y| y != v.yanked);
        if let Some(d) = deprecated {
            v.deprecated = d;
        }
        if let Some(y) = yanked {
            v.yanked = y;
        }
        if changed {
            v.updated_ms = now;
            tx.update(&VERSIONS, &ver_row(&cur, &v, row.version)?)
                .await?;
            Self::announce(
                &mut tx,
                "package.version_updated",
                "package.version.update",
                owner,
                &cur,
                json!({"version": v.version, "version_id": v.id, "deprecated": v.deprecated, "yanked": v.yanked}),
            )
            .await?;
            tx.commit().await?;
        }
        Ok(v)
    }

    /// Provenance of a version; `NOT_FOUND` when none was recorded.
    pub async fn provenance(
        &self,
        cx: &Cx,
        p: &PackageRec,
        reference: &str,
    ) -> Result<PackageVersionProvenance> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((v, _)) = Self::resolve_in(&mut tx, p, reference).await? else {
            return Err(ForgeError::not_found(format!("version {reference}")));
        };
        match tx.get(&PROVENANCE, &p.id, &v.id).await? {
            Some(r) => r.decode(),
            None => Err(ForgeError::not_found("version provenance")),
        }
    }

    // ---------- downloads and usage ----------

    /// Counts one download of a version.
    pub async fn record_download(&self, cx: &Cx, p: &PackageRec, version_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (y, m, d) = civil((tx.now_ms() / 86_400_000) as i64);
        tx.add_n1(&COUNTS, &p.id, "", 1).await?;
        tx.add_n1(&COUNTS, &p.id, version_id, 1).await?;
        tx.add_n1(
            &DAILY,
            &p.id,
            &format!("{y:04}{m:02}{d:02}:{version_id}"),
            1,
        )
        .await?;
        tx.commit().await
    }

    /// Download counts and storage; `since_ms` limits downloads to a period.
    pub async fn usage(
        &self,
        cx: &Cx,
        p: &PackageRec,
        since_ms: Option<u64>,
    ) -> Result<PackageUsage> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let vrows = tx
            .scan(&VERSIONS, &Scan::scope(&p.id, 10_000).desc())
            .await?;
        let mut versions = Vec::new();
        let mut storage = 0u64;
        let mut total = 0u64;
        let daily = match since_ms {
            Some(_) => tx.scan(&DAILY, &Scan::scope(&p.id, 10_000)).await?,
            None => Vec::new(),
        };
        let since_key = since_ms.map(|ms| {
            let (y, m, d) = civil((ms / 86_400_000) as i64);
            format!("{y:04}{m:02}{d:02}")
        });
        for r in vrows {
            let v: VersionRec = r.decode()?;
            if v.deleted_ms != 0 {
                continue;
            }
            storage += v.size_bytes;
            let n = match &since_key {
                None => Self::counter(&mut tx, &p.id, &v.id).await?,
                Some(s) => daily
                    .iter()
                    .filter(|r| {
                        r.id.split_once(':')
                            .is_some_and(|(day, vid)| vid == v.id && day >= s.as_str())
                    })
                    .map(|r| r.n1.unwrap_or(0).max(0) as u64)
                    .sum(),
            };
            total += n;
            versions.push(PackageVersionUsage {
                version_id: v.id,
                version: v.version,
                download_count: n,
            });
        }
        if since_key.is_none() {
            total = Self::counter(&mut tx, &p.id, "").await?;
        }
        Ok(PackageUsage {
            package_id: p.id.clone(),
            download_count: total,
            storage_bytes: storage,
            versions,
        })
    }

    // ---------- public shapes ----------

    /// The registry URL of a package.
    pub fn registry_url(&self, owner: &str, eco: Eco, name: &str) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let host = base.split("://").nth(1).unwrap_or(base);
        match eco {
            Eco::Container => format!("{host}/{owner}/{name}"),
            Eco::Npm => format!("{base}/npm/{owner}/"),
            Eco::Maven => format!("{base}/maven/{owner}/"),
            Eco::Nuget => format!("{base}/nuget/{owner}/v3/index.json"),
            Eco::Rubygems => format!("{base}/rubygems/{owner}/"),
            Eco::Cargo => format!("sparse+{base}/cargo/{owner}/index/"),
        }
    }

    /// The `bee.v1.Package` of a record.
    pub async fn package_proto(&self, cx: &Cx, p: &PackageRec) -> Result<Package> {
        let owner = self.owner_of(cx, p).await?;
        let eco = p.eco()?;
        let repo = if p.repo_id.is_empty() {
            None
        } else {
            Some(match self.orgs.repo_any(cx, &p.repo_id).await {
                Ok(r) => RepoRef {
                    repo: Some(repo_ref::Repo::FullName(r.full_name)),
                },
                Err(_) => RepoRef {
                    repo: Some(repo_ref::Repo::Id(p.repo_id.clone())),
                },
            })
        };
        let downloads = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::counter(&mut tx, &p.id, "").await?
        };
        Ok(Package {
            id: p.id.clone(),
            owner: owner.name.clone(),
            owner_kind: p.owner_kind,
            ecosystem: p.ecosystem,
            name: p.name.clone(),
            visibility: p.visibility,
            repo,
            inherit_access_from_repo: p.inherit_access,
            description: p.description.clone(),
            version_count: p.version_count,
            download_count: downloads,
            html_url: format!(
                "{}/{}/-/packages/{}/{}",
                self.config.base_url.trim_end_matches('/'),
                owner.name,
                eco.slug(),
                p.name
            ),
            registry_url: self.registry_url(&owner.name, eco, &p.name),
            created_at: Some(ts(p.created_ms)),
            updated_at: Some(ts(p.updated_ms)),
            deleted_at: (p.deleted_ms != 0).then(|| ts(p.deleted_ms)),
        })
    }

    /// The `bee.v1.PackageVersion` of a record, with signed download URLs on request.
    pub async fn version_proto(
        &self,
        cx: &Cx,
        p: &PackageRec,
        v: &VersionRec,
        urls: bool,
    ) -> Result<PackageVersion> {
        let files = self.files(cx, v).await?;
        let tags = self.version_tags(cx, p, &v.id).await?;
        let downloads = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::counter(&mut tx, &p.id, &v.id).await?
        };
        let now = self.db.now_ms();
        Ok(PackageVersion {
            id: v.id.clone(),
            package_id: v.package_id.clone(),
            version: v.version.clone(),
            digest: v.digest.clone(),
            tags,
            files: files
                .iter()
                .map(|f| PackageFile {
                    name: f.name.clone(),
                    size_bytes: f.size,
                    digest: f.digest.clone(),
                    media_type: f.media_type.clone(),
                    download_url: if urls {
                        self.ports.signer.download(&f.blob_key, now).url
                    } else {
                        String::new()
                    },
                })
                .collect(),
            platforms: v
                .platforms
                .iter()
                .map(|pl| PackagePlatform {
                    os: pl.os.clone(),
                    arch: pl.arch.clone(),
                    variant: pl.variant.clone(),
                    digest: pl.digest.clone(),
                })
                .collect(),
            metadata_json: v.metadata_json.clone(),
            size_bytes: v.size_bytes,
            download_count: downloads,
            publisher: Some(Principal {
                id: v.publisher_id.clone(),
                kind: PrincipalKind::try_from(v.publisher_kind).unwrap_or_default() as i32,
                ..Default::default()
            }),
            source_changeset: (!v.changeset.is_empty()).then(|| ObjectId {
                raw: v.changeset.clone(),
            }),
            workflow_run_id: v.run_id.clone(),
            has_provenance: v.has_provenance,
            created_at: Some(ts(v.created_ms)),
            updated_at: Some(ts(v.updated_ms)),
            deleted_at: (v.deleted_ms != 0).then(|| ts(v.deleted_ms)),
        })
    }

    /// Reads the content of a stored file from the blob port.
    pub async fn read_file(&self, f: &FileRec) -> Result<bytes::Bytes> {
        self.ports.blobs.get(&f.blob_key).await?.ok_or_else(|| {
            ForgeError::new(
                bee_proto::v1::ErrorReason::Integrity,
                format!("blob for `{}` is missing from the store", f.name),
            )
            .with_subcode("blob_missing")
        })
    }

    pub(crate) async fn owner_settings_in(
        tx: &mut Tx,
        owner_id: &str,
    ) -> Result<super::model::OwnerSettingsRec> {
        match tx.get(&OWNER, owner_id, "settings").await? {
            Some(r) => r.decode(),
            None => Ok(Default::default()),
        }
    }

    pub(crate) fn upload_ms(&self) -> u64 {
        self.config.upload_ttl_ms
    }
}
