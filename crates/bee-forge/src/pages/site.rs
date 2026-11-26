//! The site record of a repo: create, update, delete, and its public shapes.

use bee_proto::v1::{
    ObjectId, PagesAccess, PagesBuildType, PagesCertificate, PagesCertificateState, PagesSite,
    PagesSiteStatus, PagesSource, Repo, SubjectKind, Visibility,
};
use serde_json::json;

use super::model::{DeploymentRec, OrgSettingsRec, SiteRec};
use super::{DEPLOYMENTS, HOSTS, OWNERS, Pages, SITES, SURFACE};
use crate::community::common::{draft, emit, repo_ref_id};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan, Tx};

/// Normalizes a site folder: `/` or `/docs/sub`, no `..`, no empty components.
pub fn normalize_source_path(p: &str) -> Result<String> {
    let p = if p.is_empty() { "/" } else { p };
    if !p.starts_with('/') {
        return Err(ForgeError::invalid(
            "bad_path",
            "the source path must start with `/`",
        ));
    }
    let mut parts = Vec::new();
    for c in p.split('/').filter(|c| !c.is_empty()) {
        if c == "." || c == ".." || c.contains('\0') || c.contains('\\') {
            return Err(ForgeError::invalid(
                "bad_path",
                format!("`{p}` is not a valid source path"),
            ));
        }
        parts.push(c);
    }
    Ok(format!("/{}", parts.join("/")))
}

pub(crate) async fn announce(
    tx: &mut Tx,
    kind: &str,
    audit: &str,
    org_id: &str,
    repo_id: &str,
    subject_id: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let subject = bee_proto::v1::SubjectRef {
        kind: SubjectKind::Deployment as i32,
        repo: repo_ref_id(repo_id),
        id: subject_id.to_string(),
    };
    emit(tx, kind, org_id, repo_id, subject, payload.clone()).await?;
    tx.audit(draft(
        audit,
        org_id,
        repo_id,
        SubjectKind::Deployment,
        subject_id,
        &payload,
    ))
    .await
}

pub(crate) fn build_type(n: i32) -> PagesBuildType {
    PagesBuildType::try_from(n).unwrap_or(PagesBuildType::Ref)
}

impl Pages {
    pub(crate) async fn load_site(
        tx: &mut Tx,
        org_id: &str,
        repo_id: &str,
    ) -> Result<Option<(SiteRec, Row)>> {
        match tx.get(&SITES, org_id, repo_id).await? {
            Some(r) => Ok(Some((r.decode()?, r))),
            None => Ok(None),
        }
    }

    /// The site of `repo`; `NOT_FOUND` when it has none.
    pub async fn site(&self, cx: &Cx, repo: &Repo) -> Result<SiteRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load_site(&mut tx, &repo.org_id, &repo.id)
            .await?
            .map(|(s, _)| s)
            .ok_or_else(|| ForgeError::not_found(format!("Pages site of {}", repo.full_name)))
    }

    /// The org's Pages settings.
    pub async fn org_settings(&self, cx: &Cx, org_id: &str) -> Result<OrgSettingsRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::org_settings_in(&mut tx, org_id).await
    }

    pub(crate) async fn org_settings_in(tx: &mut Tx, org_id: &str) -> Result<OrgSettingsRec> {
        match tx.get(&OWNERS, org_id, "settings").await? {
            Some(r) => r.decode(),
            None => Ok(Default::default()),
        }
    }

    /// Forbids or allows public sites for the org.
    pub async fn set_forbid_public(&self, cx: &Cx, org_id: &str, forbid: bool) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.put(
            &OWNERS,
            &Row::new(org_id, "settings").msg(&OrgSettingsRec {
                forbid_public: forbid,
            }),
        )
        .await?;
        tx.audit(draft(
            "pages.settings.update",
            org_id,
            "",
            SubjectKind::Org,
            org_id,
            &json!({"forbid_public": forbid}),
        ))
        .await?;
        tx.commit().await
    }

    fn public_denied(org_name: &str) -> ForgeError {
        ForgeError::policy_denied(
            "pages.forbid_public",
            vec!["access:public".into()],
            format!("{org_name} does not allow public Pages sites"),
        )
    }

    /// Creates a repo's site. `ALREADY_EXISTS` when it has one, `POLICY_DENIED` for a public
    /// site in an org that forbids them.
    pub async fn create_site(
        &self,
        cx: &Cx,
        repo: &Repo,
        build: PagesBuildType,
        source: Option<PagesSource>,
        access: PagesAccess,
    ) -> Result<SiteRec> {
        let build = if build == PagesBuildType::Unspecified {
            PagesBuildType::Ref
        } else {
            build
        };
        let access = match access {
            PagesAccess::Unspecified => {
                if Visibility::try_from(repo.visibility).unwrap_or_default() == Visibility::Public {
                    PagesAccess::Public
                } else {
                    PagesAccess::Private
                }
            }
            a => a,
        };
        let (sref, spath) = match source {
            Some(s) => (
                if s.r#ref.is_empty() {
                    repo.default_ref.clone()
                } else {
                    s.r#ref
                },
                normalize_source_path(&s.path)?,
            ),
            None => (repo.default_ref.clone(), "/".to_string()),
        };
        let sref = if sref.is_empty() {
            "main".to_string()
        } else {
            sref
        };
        let org = self.orgs.org_any(cx, &repo.org_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if Self::load_site(&mut tx, &repo.org_id, &repo.id)
            .await?
            .is_some()
        {
            return Err(ForgeError::already_exists(format!(
                "Pages site of {}",
                repo.full_name
            )));
        }
        if access == PagesAccess::Public
            && Self::org_settings_in(&mut tx, &repo.org_id)
                .await?
                .forbid_public
        {
            return Err(Self::public_denied(&org.name));
        }
        let now = tx.now_ms();
        let s = SiteRec {
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            build_type: build as i32,
            source_ref: sref,
            source_path: spath,
            access: access as i32,
            cert_state: PagesCertificateState::None as i32,
            status: PagesSiteStatus::None as i32,
            created_ms: now,
            updated_ms: now,
            preview_ttl_days: self.config.preview_ttl_days,
            org_name: org.name,
            repo_name: repo.name.clone(),
            ..Default::default()
        };
        tx.insert(&SITES, &Row::new(&repo.org_id, &repo.id).msg(&s))
            .await?;
        announce(
            &mut tx,
            "pages.site_created",
            "pages.site.create",
            &repo.org_id,
            &repo.id,
            &repo.id,
            json!({"repo": repo.full_name, "build_type": s.build_type, "access": s.access}),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Updates build type, source, access and HTTPS enforcement (unset fields keep their value).
    /// `INVALID_STATE` when HTTPS is enforced for a custom domain without an issued certificate.
    pub async fn update_site(
        &self,
        cx: &Cx,
        repo: &Repo,
        build: Option<PagesBuildType>,
        source: Option<PagesSource>,
        access: Option<PagesAccess>,
        enforce_https: Option<bool>,
    ) -> Result<SiteRec> {
        let org_name = self.orgs.org_any(cx, &repo.org_id).await?.name;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut s, row)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Err(ForgeError::not_found(format!(
                "Pages site of {}",
                repo.full_name
            )));
        };
        if let Some(b) = build.filter(|b| *b != PagesBuildType::Unspecified) {
            s.build_type = b as i32;
        }
        if let Some(src) = source {
            if !src.r#ref.is_empty() {
                s.source_ref = src.r#ref;
            }
            if !src.path.is_empty() {
                s.source_path = normalize_source_path(&src.path)?;
            }
        }
        if let Some(a) = access.filter(|a| *a != PagesAccess::Unspecified) {
            if a == PagesAccess::Public
                && s.access != PagesAccess::Public as i32
                && Self::org_settings_in(&mut tx, &repo.org_id)
                    .await?
                    .forbid_public
            {
                return Err(Self::public_denied(&org_name));
            }
            s.access = a as i32;
        }
        if let Some(e) = enforce_https {
            if e && !s.custom_domain.is_empty()
                && s.cert_state != PagesCertificateState::Issued as i32
            {
                return Err(ForgeError::invalid_state(
                    "HTTPS cannot be enforced before the certificate of the custom domain is issued",
                ));
            }
            s.enforce_https = e;
        }
        s.updated_ms = tx.now_ms();
        tx.update(
            &SITES,
            &Row::new(&repo.org_id, &repo.id)
                .msg(&s)
                .version(row.version),
        )
        .await?;
        announce(
            &mut tx,
            "pages.site_updated",
            "pages.site.update",
            &repo.org_id,
            &repo.id,
            &repo.id,
            json!({"repo": repo.full_name, "build_type": s.build_type, "access": s.access, "enforce_https": s.enforce_https}),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Unpublishes and deletes the site with every deployment, preview and host claim.
    pub async fn delete_site(&self, cx: &Cx, repo: &Repo) -> Result<()> {
        let mut dropped = Vec::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((s, _)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Err(ForgeError::not_found(format!(
                "Pages site of {}",
                repo.full_name
            )));
        };
        let deployments = tx
            .scan(&DEPLOYMENTS, &Scan::scope(&repo.id, 10_000))
            .await?;
        for r in deployments {
            let d: DeploymentRec = r.decode()?;
            if !d.host.is_empty() {
                tx.delete(&HOSTS, "", &d.host).await?;
            }
            if !d.manifest_id.is_empty() {
                Self::release_manifest(&mut tx, &repo.id, &d.manifest_id, &mut dropped).await?;
            }
            tx.delete(&DEPLOYMENTS, &repo.id, &d.id).await?;
        }
        if !s.custom_domain.is_empty() {
            tx.delete(&HOSTS, "", &s.custom_domain).await?;
        }
        tx.delete(&SITES, &repo.org_id, &repo.id).await?;
        announce(
            &mut tx,
            "pages.site_deleted",
            "pages.site.delete",
            &repo.org_id,
            &repo.id,
            &repo.id,
            json!({"repo": repo.full_name}),
        )
        .await?;
        tx.commit().await?;
        for k in dropped {
            let _ = self.blobs.delete(&k).await;
        }
        Ok(())
    }

    /// The default URL of a site: `https://<org>.<pages domain>/<repo>/`.
    pub fn default_url(&self, s: &SiteRec) -> String {
        format!(
            "https://{}.{}/{}/",
            s.org_name, self.config.pages_domain, s.repo_name
        )
    }

    /// The public URL: the custom domain when one is set, else the default URL.
    pub fn site_url(&self, s: &SiteRec) -> String {
        if s.custom_domain.is_empty() {
            self.default_url(s)
        } else {
            let scheme = if s.cert_state == PagesCertificateState::Issued as i32 {
                "https"
            } else {
                "http"
            };
            format!("{scheme}://{}/", s.custom_domain)
        }
    }

    /// The `bee.v1.PagesSite` of a record.
    pub fn site_proto(&self, s: &SiteRec) -> PagesSite {
        PagesSite {
            repo: repo_ref_id(&s.repo_id),
            url: self.site_url(s),
            status: s.status,
            build_type: s.build_type,
            source: Some(PagesSource {
                r#ref: s.source_ref.clone(),
                path: s.source_path.clone(),
            }),
            custom_domain: s.custom_domain.clone(),
            enforce_https: s.enforce_https,
            certificate: Some(PagesCertificate {
                state: s.cert_state,
                domains: s.cert_domains.clone(),
                error_message: s.cert_error.clone(),
                issued_at: (s.cert_issued_ms != 0).then(|| ts(s.cert_issued_ms)),
                expires_at: (s.cert_expires_ms != 0).then(|| ts(s.cert_expires_ms)),
            }),
            access: s.access,
            current_deployment_id: s.current_deployment_id.clone(),
            served_tree: (!s.served_tree.is_empty()).then(|| ObjectId {
                raw: s.served_tree.clone(),
            }),
            created_at: Some(ts(s.created_ms)),
            updated_at: Some(ts(s.updated_ms)),
            previews_enabled: !s.previews_off,
            preview_ttl_days: s.preview_ttl_days,
        }
    }

    /// Turns per-CR previews on or off and sets how long they live (0 keeps the default).
    pub async fn set_preview_settings(
        &self,
        cx: &Cx,
        repo: &Repo,
        enabled: bool,
        ttl_days: u32,
    ) -> Result<SiteRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut s, row)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Err(ForgeError::not_found(format!(
                "Pages site of {}",
                repo.full_name
            )));
        };
        s.previews_off = !enabled;
        if ttl_days > 0 {
            s.preview_ttl_days = ttl_days;
        }
        s.updated_ms = tx.now_ms();
        tx.update(
            &SITES,
            &Row::new(&repo.org_id, &repo.id)
                .msg(&s)
                .version(row.version),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }
}
