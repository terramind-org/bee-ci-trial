//! Builds and deployments: content-addressed manifests, the atomic switch of the live
//! deployment, preview deployments (per change request or per workflow artifact) and their
//! expiry.
//!
//! A deployment is built entirely before it goes live: files are written to the blob store
//! under their SHA-256, the manifest is a deterministic function of the file list (sorted
//! paths, content ids, sizes, redirect rules), and the site's `current_deployment_id` flips in
//! the same transaction that records the deployment. A failed build leaves the previous site
//! serving. Builds run to completion inside the call, so a deployment is never observed in
//! `QUEUED`, `BUILDING` or `DEPLOYING`.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{
    ObjectId, PagesBuildType, PagesDeployment, PagesDeploymentState, PagesSiteStatus, Principal,
    PrincipalKind, Repo,
};
use bytes::Bytes;
use serde_json::json;

use super::model::{DeploymentRec, EntryRec, HostRec, ManifestRec, RedirectRec, SiteRec};
use super::site::{announce, build_type};
use super::{DEPLOYMENTS, FILEREFS, HOSTS, MANIFESTS, Pages, SITES, SURFACE};
use crate::community::common::{paged, repo_ref_id};
use crate::community::ports::VcsRepo;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::packages::archive::unpack_artifact;
use crate::packages::hash::sha256_hex;
use crate::store::{Row, Scan, Tx};

/// Options of a deployment.
#[derive(Clone, Debug, Default)]
pub struct DeployOptions {
    /// Environment name (`github-pages` when empty).
    pub environment: String,
    /// Deploy to a preview host without replacing the live site.
    pub preview: bool,
    /// Rebuild even when the ref has not moved.
    pub force: bool,
}

/// The content type served for a file name.
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "xml" => "application/xml",
        "txt" | "md" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "csv" => "text/csv; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// Parses a `_redirects` file: `from to [status[!]]` per line, `#` comments. Statuses: 301,
/// 302, 303, 307, 308 and 200 (rewrite); default 301; a trailing `!` forces the rule even when
/// a file exists. At most 1000 rules.
pub fn parse_redirects(text: &str) -> Result<Vec<RedirectRec>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let bad = |why: &str| {
            ForgeError::invalid("bad_redirects", format!("_redirects line {}: {why}", i + 1))
        };
        if parts.len() < 2 || parts.len() > 3 {
            return Err(bad("expected `from to [status]`"));
        }
        if !parts[0].starts_with('/') {
            return Err(bad("the source must start with `/`"));
        }
        if !(parts[1].starts_with('/')
            || parts[1].starts_with("http://")
            || parts[1].starts_with("https://"))
        {
            return Err(bad("the target must be a path or an absolute URL"));
        }
        let (status, force) = match parts.get(2) {
            None => (301, false),
            Some(s) => {
                let force = s.ends_with('!');
                let n: u32 = s
                    .trim_end_matches('!')
                    .parse()
                    .map_err(|_| bad("the status is not a number"))?;
                if ![200, 301, 302, 303, 307, 308].contains(&n) {
                    return Err(bad("the status must be 200, 301, 302, 303, 307 or 308"));
                }
                (n, force)
            }
        };
        out.push(RedirectRec {
            from: parts[0].to_string(),
            to: parts[1].to_string(),
            status,
            force,
        });
        if out.len() > 1000 {
            return Err(bad("more than 1000 rules"));
        }
    }
    Ok(out)
}

/// The manifest id: SHA-256 over the canonical file list and rules.
pub fn manifest_id(entries: &[EntryRec], redirects: &[RedirectRec]) -> String {
    let mut s = String::new();
    for e in entries {
        s.push_str(&format!("F\0{}\0{}\0{}\n", e.path, e.blob, e.size));
    }
    for r in redirects {
        s.push_str(&format!(
            "R\0{}\0{}\0{}\0{}\n",
            r.from, r.to, r.status, r.force
        ));
    }
    sha256_hex(s.as_bytes())
}

/// What a deployment source yields.
struct Built {
    manifest: ManifestRec,
    files: Vec<(String, Bytes)>,
}

fn state_of(n: i32) -> PagesDeploymentState {
    PagesDeploymentState::try_from(n).unwrap_or(PagesDeploymentState::Succeeded)
}

impl Pages {
    /// Hashes the files into a manifest (not yet stored). Enforces the site limits and parses
    /// `_redirects`.
    fn build_manifest(&self, files: Vec<(String, Bytes)>) -> Result<Built> {
        if files.len() > self.config.max_files {
            return Err(ForgeError::too_large(
                "files in site",
                self.config.max_files as u64,
                files.len() as u64,
            ));
        }
        let mut total = 0u64;
        let mut by_path: BTreeMap<String, Bytes> = BTreeMap::new();
        for (p, d) in files {
            if d.len() as u64 > self.config.max_file_bytes {
                return Err(ForgeError::too_large(
                    "file size",
                    self.config.max_file_bytes,
                    d.len() as u64,
                ));
            }
            total += d.len() as u64;
            by_path.insert(p, d);
        }
        if total > self.config.max_site_bytes {
            return Err(ForgeError::too_large(
                "site size",
                self.config.max_site_bytes,
                total,
            ));
        }
        let redirects = match by_path.remove("_redirects") {
            Some(d) => parse_redirects(&String::from_utf8_lossy(&d))?,
            None => Vec::new(),
        };
        let mut entries = Vec::new();
        let mut keep = Vec::new();
        for (path, data) in by_path {
            let blob = sha256_hex(&data);
            entries.push(EntryRec {
                content_type: content_type(&path).to_string(),
                size: data.len() as u64,
                blob,
                path: path.clone(),
            });
            keep.push((path, data));
        }
        let id = manifest_id(&entries, &redirects);
        Ok(Built {
            manifest: ManifestRec {
                id,
                entries,
                redirects,
                total_bytes: total,
            },
            files: keep,
        })
    }

    /// Writes new content blobs (content-addressed, so existing ones are skipped).
    async fn store_blobs(&self, repo_id: &str, b: &Built) -> Result<()> {
        let mut done = BTreeSet::new();
        for ((_, data), e) in b.files.iter().zip(&b.manifest.entries) {
            if !done.insert(e.blob.clone()) {
                continue;
            }
            let key = Self::blob_key(repo_id, &e.blob);
            if self.blobs.head(&key).await?.is_none() {
                self.blobs.put(&key, data.clone()).await?;
            }
        }
        Ok(())
    }

    /// Drops one reference to a manifest; at zero the manifest row goes and so do references to
    /// its files, whose unreferenced blob keys are pushed to `dropped`.
    pub(crate) async fn release_manifest(
        tx: &mut Tx,
        repo_id: &str,
        manifest_id: &str,
        dropped: &mut Vec<String>,
    ) -> Result<()> {
        let Some(row) = tx.get(&MANIFESTS, repo_id, manifest_id).await? else {
            return Ok(());
        };
        let n = row.n1.unwrap_or(1) - 1;
        if n > 0 {
            tx.add_n1(&MANIFESTS, repo_id, manifest_id, -1).await?;
            return Ok(());
        }
        let m: ManifestRec = row.decode()?;
        tx.delete(&MANIFESTS, repo_id, manifest_id).await?;
        let distinct: BTreeSet<&str> = m.entries.iter().map(|e| e.blob.as_str()).collect();
        for b in distinct {
            if tx.add_n1(&FILEREFS, repo_id, b, -1).await? <= 0 {
                tx.delete(&FILEREFS, repo_id, b).await?;
                dropped.push(Self::blob_key(repo_id, b));
            }
        }
        Ok(())
    }

    async fn acquire_manifest(tx: &mut Tx, repo_id: &str, m: &ManifestRec) -> Result<()> {
        if tx.get(&MANIFESTS, repo_id, &m.id).await?.is_some() {
            tx.add_n1(&MANIFESTS, repo_id, &m.id, 1).await?;
            return Ok(());
        }
        tx.insert(&MANIFESTS, &Row::new(repo_id, &m.id).n1(1).msg(m))
            .await?;
        let distinct: BTreeSet<&str> = m.entries.iter().map(|e| e.blob.as_str()).collect();
        for b in distinct {
            tx.add_n1(&FILEREFS, repo_id, b, 1).await?;
        }
        Ok(())
    }

    /// The host name of the preview of change request `n`.
    pub fn preview_host(&self, s: &SiteRec, n: u64) -> String {
        format!(
            "{}-pr-{n}.{}.{}",
            s.repo_name, s.org_name, self.config.pages_domain
        )
        .to_lowercase()
    }

    #[allow(clippy::too_many_arguments)]
    fn new_deployment(
        &self,
        tx: &Tx,
        cx: &Cx,
        s: &SiteRec,
        git_ref: &str,
        changeset: &[u8],
        opts: &DeployOptions,
    ) -> Result<DeploymentRec> {
        let now = tx.now_ms();
        Ok(DeploymentRec {
            id: tx.new_id()?,
            repo_id: s.repo_id.clone(),
            state: PagesDeploymentState::Building as i32,
            build_type: s.build_type,
            git_ref: git_ref.to_string(),
            changeset: changeset.to_vec(),
            creator: cx.actor_id.clone(),
            creator_kind: cx.actor_kind as i32,
            created_ms: now,
            updated_ms: now,
            preview: opts.preview,
            environment: if opts.environment.is_empty() {
                "github-pages".into()
            } else {
                opts.environment.clone()
            },
            ..Default::default()
        })
    }

    /// Records a failed build: the deployment row, the site's `ERRORED` status (live builds)
    /// and the `page_build.completed` event. The previous deployment keeps serving.
    async fn record_failure(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut d: DeploymentRec,
        message: &str,
    ) -> Result<DeploymentRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        d.state = PagesDeploymentState::Failed as i32;
        d.error_message = message.to_string();
        d.updated_ms = now;
        d.duration_ms = now.saturating_sub(d.created_ms);
        tx.insert(
            &DEPLOYMENTS,
            &Row::new(&repo.id, &d.id)
                .k1(if d.preview { "preview" } else { "live" })
                .msg(&d),
        )
        .await?;
        if !d.preview
            && let Some((mut s, row)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await?
        {
            s.status = PagesSiteStatus::Errored as i32;
            s.updated_ms = now;
            tx.update(
                &SITES,
                &Row::new(&repo.org_id, &repo.id)
                    .msg(&s)
                    .version(row.version),
            )
            .await?;
        }
        announce(
            &mut tx,
            "pages.deployment_failed",
            "pages.deployment.fail",
            &repo.org_id,
            &repo.id,
            &d.id,
            json!({"repo": repo.full_name, "deployment_id": d.id, "error": message}),
        )
        .await?;
        announce(
            &mut tx,
            "page_build.completed",
            "pages.build.complete",
            &repo.org_id,
            &repo.id,
            &d.id,
            json!({"repo": repo.full_name, "deployment_id": d.id, "status": "errored", "error": message}),
        )
        .await?;
        tx.commit().await?;
        Ok(d)
    }

    /// Stores a built site and, in one transaction, records the deployment and switches the live
    /// site (or registers the preview host).
    async fn go_live(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut d: DeploymentRec,
        built: Built,
        tree_id: Vec<u8>,
        cr: Option<(u64, u64)>,
    ) -> Result<DeploymentRec> {
        self.store_blobs(&repo.id, &built).await?;
        let mut dropped = Vec::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut s, srow)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Err(ForgeError::not_found(format!(
                "Pages site of {}",
                repo.full_name
            )));
        };
        Self::acquire_manifest(&mut tx, &repo.id, &built.manifest).await?;
        d.manifest_id = built.manifest.id.clone();
        d.state = PagesDeploymentState::Succeeded as i32;
        d.updated_ms = now;
        d.duration_ms = now.saturating_sub(d.created_ms);
        let kind = if d.preview { "preview" } else { "live" };
        let mut row = Row::new(&repo.id, &d.id).k1(kind);
        if d.preview {
            let (n, ttl_days) = cr.unwrap_or((0, u64::from(s.preview_ttl_days)));
            d.cr_number = n;
            d.expires_ms = now + ttl_days.max(1) * 86_400_000;
            d.host = if n > 0 {
                self.preview_host(&s, n)
            } else {
                format!(
                    "{}-preview-{}.{}.{}",
                    s.repo_name,
                    d.id[d.id.len().saturating_sub(8)..].to_lowercase(),
                    s.org_name,
                    self.config.pages_domain
                )
                .to_lowercase()
            };
            d.preview_url = format!("https://{}/", d.host);
            d.page_url = d.preview_url.clone();
            row = row.n1(d.expires_ms as i64);
            if let Some(old) = tx.get(&HOSTS, "", &d.host).await? {
                let h: HostRec = old.decode()?;
                if let Some(od) = tx.get(&DEPLOYMENTS, &repo.id, &h.deployment_id).await? {
                    let od: DeploymentRec = od.decode()?;
                    if !od.manifest_id.is_empty() {
                        Self::release_manifest(&mut tx, &repo.id, &od.manifest_id, &mut dropped)
                            .await?;
                    }
                    tx.delete(&DEPLOYMENTS, &repo.id, &od.id).await?;
                }
            }
            tx.put(
                &HOSTS,
                &Row::new("", &d.host).k1(&repo.id).msg(&HostRec {
                    repo_id: repo.id.clone(),
                    kind: "preview".into(),
                    deployment_id: d.id.clone(),
                }),
            )
            .await?;
        } else {
            d.page_url = self.site_url(&s);
            s.current_deployment_id = d.id.clone();
            s.served_tree = tree_id;
            s.status = PagesSiteStatus::Built as i32;
            s.updated_ms = now;
            tx.update(
                &SITES,
                &Row::new(&repo.org_id, &repo.id)
                    .msg(&s)
                    .version(srow.version),
            )
            .await?;
        }
        tx.insert(&DEPLOYMENTS, &row.msg(&d)).await?;
        if !d.preview {
            // Keep the newest deployments; the serving one is always kept.
            let all = tx
                .scan(
                    &DEPLOYMENTS,
                    &Scan::scope(&repo.id, 10_000)
                        .key(crate::store::Key::K1("live".into()))
                        .desc(),
                )
                .await?;
            for r in all.into_iter().skip(self.config.keep_deployments.max(1)) {
                let od: DeploymentRec = r.decode()?;
                if od.id == s.current_deployment_id {
                    continue;
                }
                if !od.manifest_id.is_empty() {
                    Self::release_manifest(&mut tx, &repo.id, &od.manifest_id, &mut dropped)
                        .await?;
                }
                tx.delete(&DEPLOYMENTS, &repo.id, &od.id).await?;
            }
        }
        let payload = json!({
            "repo": repo.full_name, "deployment_id": d.id, "manifest": d.manifest_id,
            "files": built.manifest.entries.len(), "bytes": built.manifest.total_bytes,
            "ref": d.git_ref, "changeset": hex(&d.changeset), "url": d.page_url, "preview": d.preview,
        });
        announce(
            &mut tx,
            if d.preview {
                "pages.preview_created"
            } else {
                "pages.deployment_succeeded"
            },
            if d.preview {
                "pages.preview.create"
            } else {
                "pages.deployment.succeed"
            },
            &repo.org_id,
            &repo.id,
            &d.id,
            payload.clone(),
        )
        .await?;
        if !d.preview {
            let mut p = payload;
            p["status"] = json!("built");
            announce(
                &mut tx,
                "page_build.completed",
                "pages.build.complete",
                &repo.org_id,
                &repo.id,
                &d.id,
                p,
            )
            .await?;
        }
        tx.commit().await?;
        for k in dropped {
            let _ = self.blobs.delete(&k).await;
        }
        Ok(d)
    }

    /// The files of `repo` at `at` under `path`, read from the VCS.
    async fn tree_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
        path: &str,
    ) -> Result<Vec<(String, Bytes)>> {
        let vr = VcsRepo::of_repo(repo);
        let prefix = path.trim_matches('/').to_string();
        let mut files = Vec::new();
        let mut total = 0u64;
        let tree = self.vcs.tree(&vr, at).await?;
        if tree.len() > self.config.max_files * 4 && prefix.is_empty() {
            return Err(ForgeError::too_large(
                "files in site",
                self.config.max_files as u64,
                tree.len() as u64,
            ));
        }
        for f in tree {
            let rel = if prefix.is_empty() {
                f.path.clone()
            } else if let Some(r) = f.path.strip_prefix(&format!("{prefix}/")) {
                r.to_string()
            } else {
                continue;
            };
            if f.size > self.config.max_file_bytes {
                return Err(ForgeError::too_large(
                    "file size",
                    self.config.max_file_bytes,
                    f.size,
                ));
            }
            total += f.size;
            if total > self.config.max_site_bytes {
                return Err(ForgeError::too_large(
                    "site size",
                    self.config.max_site_bytes,
                    total,
                ));
            }
            let data = self.vcs.read(&vr, &f.blob).await?;
            files.push((rel, Bytes::from(data)));
        }
        Ok(files)
    }

    /// Builds the configured ref and folder and deploys it now. A build problem (unknown ref,
    /// site over the limit, bad `_redirects`) is recorded as a `FAILED` deployment and returned;
    /// `INVALID_STATE` when the site uses the workflow build type. When the ref has not moved
    /// since the live deployment, that deployment is returned unless `opts.force`.
    pub async fn request_build(
        &self,
        cx: &Cx,
        repo: &Repo,
        opts: &DeployOptions,
    ) -> Result<DeploymentRec> {
        let site = self.site(cx, repo).await?;
        if build_type(site.build_type) != PagesBuildType::Ref {
            return Err(ForgeError::invalid_state(
                "the site uses the workflow build type; deploy an artifact instead",
            ));
        }
        let vr = VcsRepo::of_repo(repo);
        let draft = {
            let tx = self.db.begin(SURFACE, cx).await?;
            self.new_deployment(&tx, cx, &site, &site.source_ref, &[], opts)?
        };
        let at = match self.vcs.resolve(&vr, &site.source_ref).await {
            Ok(a) => a,
            Err(e) => {
                return self
                    .record_failure(
                        cx,
                        repo,
                        draft,
                        &format!("cannot resolve `{}`: {}", site.source_ref, e.message),
                    )
                    .await;
            }
        };
        let mut d = draft;
        d.changeset = at.raw.clone();
        if !opts.force && !site.current_deployment_id.is_empty() {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if let Some(cur) = tx
                .get(&DEPLOYMENTS, &repo.id, &site.current_deployment_id)
                .await?
            {
                let cur: DeploymentRec = cur.decode()?;
                if cur.changeset == at.raw && cur.state == PagesDeploymentState::Succeeded as i32 {
                    return Ok(cur);
                }
            }
        }
        let built = match self
            .tree_files(repo, &at, &site.source_path)
            .await
            .and_then(|f| self.build_manifest(f))
        {
            Ok(b) => b,
            Err(e) => return self.record_failure(cx, repo, d, &e.message).await,
        };
        let tree = self
            .vcs
            .tree_id(&vr, &at)
            .await
            .map(|t| t.raw)
            .unwrap_or_default();
        self.go_live(cx, repo, d, built, tree, None).await
    }

    /// Deploys a workflow artifact. `INVALID_STATE` for a REF site, `APPROVAL_REQUIRED` when the
    /// environment is protected, `NOT_FOUND` for an unknown artifact, `TOO_LARGE` over the limits.
    /// With `opts.preview` the artifact goes to a preview host and the live site is untouched.
    pub async fn create_deployment(
        &self,
        cx: &Cx,
        repo: &Repo,
        artifact_id: &str,
        changeset: Option<ObjectId>,
        opts: &DeployOptions,
    ) -> Result<DeploymentRec> {
        let site = self.site(cx, repo).await?;
        if build_type(site.build_type) != PagesBuildType::Workflow {
            return Err(ForgeError::invalid_state(
                "the site uses the ref build type; request a build instead",
            ));
        }
        if artifact_id.is_empty() {
            return Err(ForgeError::missing("artifact_id"));
        }
        let env = if opts.environment.is_empty() {
            "github-pages"
        } else {
            opts.environment.as_str()
        };
        self.ports.gate.check(cx, repo, env).await?;
        let artifact = self.ports.artifacts.fetch(repo, artifact_id).await?;
        let files: Vec<(String, Bytes)> =
            unpack_artifact(&artifact.data, self.config.max_site_bytes as usize)?
                .into_iter()
                .map(|e| (e.path, Bytes::from(e.data)))
                .collect();
        let at = changeset.or(artifact.changeset.clone());
        let mut d = {
            let tx = self.db.begin(SURFACE, cx).await?;
            self.new_deployment(
                &tx,
                cx,
                &site,
                "",
                at.as_ref().map(|c| c.raw.as_slice()).unwrap_or(&[]),
                opts,
            )?
        };
        d.artifact_id = artifact_id.to_string();
        d.run_id = artifact.run_id.clone();
        let built = self.build_manifest(files)?;
        self.go_live(cx, repo, d, built, Vec::new(), None).await
    }

    /// Builds the preview of change request `number` from its head changeset. The preview host
    /// is stable per change request: a new revision replaces the previous preview. Only REF
    /// sites with previews on are previewed this way (workflow sites deploy previews from
    /// artifacts).
    pub async fn deploy_change_preview(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
        head: &ObjectId,
    ) -> Result<DeploymentRec> {
        let site = self.site(cx, repo).await?;
        if site.previews_off {
            return Err(ForgeError::invalid_state(
                "previews are turned off for this site",
            ));
        }
        if build_type(site.build_type) != PagesBuildType::Ref {
            return Err(ForgeError::invalid_state(
                "the site uses the workflow build type; deploy an artifact with preview instead",
            ));
        }
        let opts = DeployOptions {
            preview: true,
            ..Default::default()
        };
        let mut d = {
            let tx = self.db.begin(SURFACE, cx).await?;
            self.new_deployment(&tx, cx, &site, &format!("cr/{number}"), &head.raw, &opts)?
        };
        d.cr_number = number;
        let built = match self
            .tree_files(repo, head, &site.source_path)
            .await
            .and_then(|f| self.build_manifest(f))
        {
            Ok(b) => b,
            Err(e) => return self.record_failure(cx, repo, d, &e.message).await,
        };
        let ttl = u64::from(site.preview_ttl_days);
        self.go_live(cx, repo, d, built, Vec::new(), Some((number, ttl)))
            .await
    }

    /// Removes the preview of change request `number`, when there is one. Returns whether one
    /// was removed.
    pub async fn remove_change_preview(&self, cx: &Cx, repo: &Repo, number: u64) -> Result<bool> {
        let site = match self.site(cx, repo).await {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        let host = self.preview_host(&site, number);
        self.remove_preview_host(cx, repo, &host, "pages.preview.remove")
            .await
    }

    async fn remove_preview_host(
        &self,
        cx: &Cx,
        repo: &Repo,
        host: &str,
        audit: &str,
    ) -> Result<bool> {
        let mut dropped = Vec::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(h) = tx.get(&HOSTS, "", host).await? else {
            return Ok(false);
        };
        let h: HostRec = h.decode()?;
        if let Some(r) = tx.get(&DEPLOYMENTS, &repo.id, &h.deployment_id).await? {
            let d: DeploymentRec = r.decode()?;
            if !d.manifest_id.is_empty() {
                Self::release_manifest(&mut tx, &repo.id, &d.manifest_id, &mut dropped).await?;
            }
            tx.delete(&DEPLOYMENTS, &repo.id, &d.id).await?;
        }
        tx.delete(&HOSTS, "", host).await?;
        announce(
            &mut tx,
            "pages.preview_removed",
            audit,
            &repo.org_id,
            &repo.id,
            &h.deployment_id,
            json!({"repo": repo.full_name, "host": host}),
        )
        .await?;
        tx.commit().await?;
        for k in dropped {
            let _ = self.blobs.delete(&k).await;
        }
        Ok(true)
    }

    /// Removes preview deployments past their expiry.
    pub async fn expire_previews(&self, cx: &Cx) -> Result<usize> {
        let now = self.db.now_ms() as i64;
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &DEPLOYMENTS,
                &Scan {
                    n1_min: Some(1),
                    n1_max: Some(now),
                    limit: 500,
                    ..Scan::default()
                }
                .by_n1(),
            )
            .await?
        };
        let mut n = 0;
        for r in rows {
            let d: DeploymentRec = r.decode()?;
            if !d.preview {
                continue;
            }
            let Ok(repo) = self.orgs.repo_any(cx, &d.repo_id).await else {
                continue;
            };
            if d.host.is_empty() {
                continue;
            }
            if self
                .remove_preview_host(cx, &repo, &d.host, "pages.preview.expire")
                .await?
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Lists a site's deployments, newest first.
    pub async fn deployments(
        &self,
        cx: &Cx,
        repo: &Repo,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<DeploymentRec>, String)> {
        self.site(cx, repo).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = paged(
            &mut tx,
            &DEPLOYMENTS,
            Scan::scope(&repo.id, limit).desc(),
            token,
            limit,
        )
        .await?;
        let out = rows
            .iter()
            .map(|r| r.decode())
            .collect::<Result<Vec<DeploymentRec>>>()?;
        Ok((out, next))
    }

    /// One deployment by id, or the newest live one when `id` is empty.
    pub async fn deployment(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<DeploymentRec> {
        self.site(cx, repo).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if id.is_empty() {
            let rows = tx
                .scan(
                    &DEPLOYMENTS,
                    &Scan::scope(&repo.id, 1)
                        .key(crate::store::Key::K1("live".into()))
                        .desc(),
                )
                .await?;
            return rows
                .first()
                .ok_or_else(|| ForgeError::not_found("deployment"))?
                .decode();
        }
        tx.get_msg::<DeploymentRec>(&DEPLOYMENTS, &repo.id, id)
            .await?
            .map(|(d, _)| d)
            .ok_or_else(|| ForgeError::not_found(format!("deployment {id}")))
    }

    /// Cancels a deployment in progress. Builds finish inside the call that starts them, so the
    /// only states a stored deployment has are final; `INVALID_STATE` then, `NOT_FOUND` for an
    /// unknown id.
    pub async fn cancel_deployment(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<DeploymentRec> {
        let d = self.deployment(cx, repo, id).await?;
        match state_of(d.state) {
            PagesDeploymentState::Queued
            | PagesDeploymentState::Building
            | PagesDeploymentState::Deploying => {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                let mut d = d;
                d.state = PagesDeploymentState::Cancelled as i32;
                d.updated_ms = tx.now_ms();
                tx.put(
                    &DEPLOYMENTS,
                    &Row::new(&repo.id, &d.id)
                        .k1(if d.preview { "preview" } else { "live" })
                        .msg(&d),
                )
                .await?;
                announce(
                    &mut tx,
                    "pages.deployment_cancelled",
                    "pages.deployment.cancel",
                    &repo.org_id,
                    &repo.id,
                    &d.id,
                    json!({"repo": repo.full_name, "deployment_id": d.id}),
                )
                .await?;
                tx.commit().await?;
                Ok(d)
            }
            _ => Err(ForgeError::invalid_state("the deployment has finished")),
        }
    }

    /// The `bee.v1.PagesDeployment` of a record.
    pub fn deployment_proto(&self, d: &DeploymentRec) -> PagesDeployment {
        PagesDeployment {
            id: d.id.clone(),
            repo: repo_ref_id(&d.repo_id),
            state: d.state,
            build_type: d.build_type,
            r#ref: d.git_ref.clone(),
            changeset: (!d.changeset.is_empty()).then(|| ObjectId {
                raw: d.changeset.clone(),
            }),
            artifact_id: d.artifact_id.clone(),
            workflow_run_id: d.run_id.clone(),
            deployment_id: String::new(),
            page_url: d.page_url.clone(),
            preview_url: d.preview_url.clone(),
            error_message: d.error_message.clone(),
            duration_ms: d.duration_ms,
            creator: Some(Principal {
                id: d.creator.clone(),
                kind: PrincipalKind::try_from(d.creator_kind).unwrap_or_default() as i32,
                ..Default::default()
            }),
            created_at: Some(ts(d.created_ms)),
            updated_at: Some(ts(d.updated_ms)),
            preview: d.preview,
            change_request_number: d.cr_number,
            expires_at: (d.expires_ms != 0).then(|| ts(d.expires_ms)),
        }
    }

    /// The site's live previews (deployments its preview hosts serve), newest first.
    pub async fn previews(&self, cx: &Cx, repo: &Repo) -> Result<Vec<DeploymentRec>> {
        self.site(cx, repo).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &DEPLOYMENTS,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN).desc(),
            )
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let d: DeploymentRec = r.decode()?;
            if !d.preview || d.host.is_empty() {
                continue;
            }
            if let Some(h) = tx.get(&HOSTS, "", &d.host).await? {
                let h: HostRec = h.decode()?;
                if h.deployment_id == d.id {
                    out.push(d);
                }
            }
        }
        Ok(out)
    }

    /// The manifest of a deployment.
    pub async fn manifest(&self, cx: &Cx, repo_id: &str, manifest_id: &str) -> Result<ManifestRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<ManifestRec>(&MANIFESTS, repo_id, manifest_id)
            .await?
            .map(|(m, _)| m)
            .ok_or_else(|| ForgeError::not_found("manifest"))
    }
}
