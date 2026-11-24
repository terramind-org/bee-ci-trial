//! The dependency graph of a repo: graphs at a revision (manifests parsed by `bee-deps` plus
//! submitted snapshots), manifests, the submission API, dependents, SBOM export and dependency
//! review.

use std::collections::BTreeSet;

use bee_deps::matching::{MatchOptions, match_graph};
use bee_deps::review::{ChangeType, ReviewPolicy, review};
use bee_deps::sbom::{SbomOptions, cyclonedx_json, spdx_json, to_json_string};
use bee_deps::{
    DependencyGraph, Ecosystem, ManifestInfo, Package, Purl, Relationship, Scope, SourceFile,
};
use bee_proto::v1::{
    CheckOutput, CheckStatus, Dependency, DependencyChange, DependencyChangeType,
    DependencyDependent, DependencyManifest, DependencyRelationship,
    DependencyReview as ProtoReview, DependencyReviewRange, DependencyScope, ExportSbomRequest,
    ExportSbomResponse, GetDependencyReviewRequest, ListDependenciesRequest,
    ListDependenciesResponse, ListDependentsRequest, ObjectId, Repo, SbomFormat, SecuritySeverity,
    SubjectKind, SubmitDependencySnapshotRequest, SubmitDependencySnapshotResponse,
};
use sha2::{Digest, Sha256};

use super::model::{GraphRec, PkgRec, SnapshotRec};
use super::{Change, GRAPHS, SNAPSHOTS, Security, hex_id, page_slice, record, repo_ref_of, sev};
use crate::checks::{Report, RunFilter};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::store::{Scan, Surface};

/// Most dependencies one submitted snapshot may carry.
pub const MAX_SNAPSHOT_DEPENDENCIES: usize = 20_000;
/// Largest SBOM returned inline.
pub const MAX_INLINE_SBOM: usize = 16 << 20;
/// SBOMs larger than this go through the document publisher when there is one.
pub const SIGNED_URL_THRESHOLD: usize = 1 << 20;
/// Largest dependency file read.
const MAX_FILE: u64 = 8 << 20;

const EXTENSIONS: &[&str] = &[
    ".json", ".lock", ".toml", ".txt", ".yml", ".yaml", ".mod", ".sum", ".xml", ".props",
    ".csproj", ".fsproj", ".vbproj", "Gemfile",
];

/// Whether `path` is a manifest or lockfile the detector reads.
pub fn is_dependency_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if !EXTENSIONS.iter().any(|e| base.ends_with(e)) {
        return false;
    }
    !DependencyGraph::from_files(&[SourceFile::new(path, "")])
        .manifests()
        .is_empty()
}

/// The wire form of a package.
pub(crate) fn pkg_to_proto(p: &Package, submitted: bool) -> Dependency {
    Dependency {
        package_url: p.purl.clone(),
        ecosystem: p.ecosystem.proto_name().to_string(),
        name: p.name.clone(),
        version: p.version.clone(),
        manifest_path: p.source_path.clone(),
        scope: match p.scope {
            Scope::Runtime => DependencyScope::Runtime,
            Scope::Development => DependencyScope::Development,
        } as i32,
        relationship: match p.relationship {
            Relationship::Direct => DependencyRelationship::Direct,
            Relationship::Indirect => DependencyRelationship::Indirect,
        } as i32,
        licenses: p.licenses.clone(),
        dependency_urls: p.dependencies.clone(),
        submitted,
    }
}

fn pkg_to_rec(p: &Package, submitted: bool) -> PkgRec {
    PkgRec {
        ecosystem: p.ecosystem.proto_name().to_string(),
        name: p.name.clone(),
        version: p.version.clone(),
        purl: p.purl.clone(),
        source_path: p.source_path.clone(),
        scope: match p.scope {
            Scope::Runtime => DependencyScope::Runtime,
            Scope::Development => DependencyScope::Development,
        } as i32,
        relationship: match p.relationship {
            Relationship::Direct => DependencyRelationship::Direct,
            Relationship::Indirect => DependencyRelationship::Indirect,
        } as i32,
        resolved: p.resolved,
        dependencies: p.dependencies.clone(),
        licenses: p.licenses.clone(),
        requirement: p.requirement.clone().unwrap_or_default(),
        integrity: p.integrity.clone().unwrap_or_default(),
        submitted,
    }
}

fn rec_to_pkg(r: &PkgRec) -> Option<Package> {
    let eco = Ecosystem::from_proto_name(&r.ecosystem)?;
    let mut p = Package::new(eco, &r.name, &r.version, &r.source_path);
    p.purl = r.purl.clone();
    p.scope = if r.scope == DependencyScope::Development as i32 {
        Scope::Development
    } else {
        Scope::Runtime
    };
    p.relationship = if r.relationship == DependencyRelationship::Indirect as i32 {
        Relationship::Indirect
    } else {
        Relationship::Direct
    };
    p.resolved = r.resolved;
    p.dependencies = r.dependencies.clone();
    p.licenses = r.licenses.clone();
    p.requirement = (!r.requirement.is_empty()).then(|| r.requirement.clone());
    p.integrity = (!r.integrity.is_empty()).then(|| r.integrity.clone());
    Some(p)
}

/// The graph a stored record holds.
pub(crate) fn graph_of_rec(g: &GraphRec) -> DependencyGraph {
    let packages = g.packages.iter().filter_map(rec_to_pkg).collect();
    let manifests = g
        .manifests
        .iter()
        .filter_map(|m| {
            Some(ManifestInfo {
                path: m.path.clone(),
                ecosystem: Ecosystem::from_proto_name(&m.ecosystem)?,
                is_lockfile: m.is_lockfile,
                dependency_count: m.dependency_count,
                error: None,
            })
        })
        .collect();
    DependencyGraph::new(packages, manifests)
}

fn manifest_to_proto(m: &ManifestInfo, detector: &str) -> DependencyManifest {
    DependencyManifest {
        path: m.path.clone(),
        ecosystem: m.ecosystem.proto_name().to_string(),
        is_lockfile: m.is_lockfile,
        dependency_count: m.dependency_count,
        detector: detector.to_string(),
    }
}

/// Packages and manifests of a submitted snapshot.
fn snapshot_graph_parts(s: &SnapshotRec) -> (Vec<Package>, Vec<ManifestInfo>) {
    let mut packages = Vec::new();
    let mut manifests = Vec::new();
    for m in &s.manifests {
        let eco = Ecosystem::from_proto_name(&m.ecosystem);
        let mut count = 0;
        for d in &m.dependencies {
            let Some(e) = Ecosystem::from_proto_name(&d.ecosystem)
                .or(eco)
                .or_else(|| {
                    Purl::parse(&d.package_url)
                        .ok()
                        .and_then(|p| Ecosystem::from_purl_type(&p.ty))
                })
            else {
                continue;
            };
            let mut p = Package::new(e, &d.name, &d.version, &m.path);
            if !d.package_url.is_empty() {
                p.purl = d.package_url.clone();
            }
            p.scope = if d.scope == DependencyScope::Development as i32 {
                Scope::Development
            } else {
                Scope::Runtime
            };
            p.relationship = if d.relationship == DependencyRelationship::Indirect as i32 {
                Relationship::Indirect
            } else {
                Relationship::Direct
            };
            p.dependencies = d.dependency_urls.clone();
            p.licenses = d.licenses.clone();
            packages.push(p);
            count += 1;
        }
        if let Some(e) = eco {
            manifests.push(ManifestInfo {
                path: m.path.clone(),
                ecosystem: e,
                is_lockfile: false,
                dependency_count: count,
                error: None,
            });
        }
    }
    (packages, manifests)
}

/// The names (`<ecosystem>:<name>`) a repo publishes, from its root manifests.
pub(crate) fn published_names(files: &[SourceFile]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for f in files {
        let base = f.path.rsplit('/').next().unwrap_or(&f.path);
        match base {
            "package.json" => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&f.content)
                    && let Some(n) = v.get("name").and_then(|n| n.as_str())
                {
                    out.insert(format!("npm:{n}"));
                }
            }
            "go.mod" => {
                if let Some(m) = f
                    .content
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("module "))
                {
                    out.insert(format!("go:{}", m.trim().trim_matches('"')));
                }
            }
            "Cargo.toml" => {
                if let Some(n) = toml_name(&f.content, &["package"]) {
                    out.insert(format!("cargo:{n}"));
                }
            }
            "pyproject.toml" => {
                if let Some(n) = toml_name(&f.content, &["project", "tool.poetry"]) {
                    out.insert(format!("pip:{}", Ecosystem::PyPI.normalize_name(&n)));
                }
            }
            _ => {}
        }
    }
    out.into_iter().collect()
}

fn toml_name(text: &str, sections: &[&str]) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let l = line.trim();
        if let Some(h) = l.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            in_section = sections.contains(&h.trim());
            continue;
        }
        if in_section
            && let Some(rest) = l.strip_prefix("name")
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            return Some(v.trim().trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

impl Security {
    /// The dependency files of a revision, with a digest of their content. Lists the whole
    /// tree; [`Security::read_dependency_files`] reads known paths only.
    pub(crate) async fn dependency_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
    ) -> Result<(Vec<SourceFile>, String)> {
        let listing = self.ports.vcs.list_files(repo, at).await?;
        let paths = listing
            .into_iter()
            .filter(|f| f.size <= MAX_FILE && is_dependency_file(&f.path))
            .map(|f| f.path);
        self.read_dependency_files(repo, at, paths).await
    }

    /// The files at `paths` (sorted by path, ascending) that exist at `at`, are dependency
    /// files, fit [`MAX_FILE`] and are text, with the digest [`Security::dependency_files`]
    /// computes for the same files.
    pub(crate) async fn read_dependency_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
        paths: impl IntoIterator<Item = String>,
    ) -> Result<(Vec<SourceFile>, String)> {
        let mut files = Vec::new();
        let mut h = Sha256::new();
        for path in paths {
            if !is_dependency_file(&path) {
                continue;
            }
            let Some(bytes) = self.ports.vcs.read_file(repo, at, &path).await? else {
                continue;
            };
            if bytes.len() as u64 > MAX_FILE {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            h.update(path.as_bytes());
            h.update([0]);
            h.update(Sha256::digest(text.as_bytes()));
            files.push(SourceFile {
                path,
                content: text,
            });
        }
        Ok((files, crate::ids::hex(&h.finalize())))
    }

    /// The submitted snapshots of a repo.
    pub(crate) async fn snapshots(&self, cx: &Cx, repo: &Repo) -> Result<Vec<SnapshotRec>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .scan_msgs::<SnapshotRec>(&SNAPSHOTS, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .collect())
    }

    /// The submitted snapshots that apply at `at`: those for exactly this changeset, and the
    /// default branch's when `at` is the default branch tip.
    async fn applicable_snapshots(
        &self,
        cx: &Cx,
        repo: &Repo,
        at: &ObjectId,
    ) -> Result<Vec<SnapshotRec>> {
        let tip = self.ports.vcs.resolve(repo, "").await.ok();
        let is_tip = tip.as_ref() == Some(at);
        Ok(self
            .snapshots(cx, repo)
            .await?
            .into_iter()
            .filter(|s| s.changeset == at.raw || (is_tip && s.ref_name == repo.default_ref))
            .collect())
    }

    /// The digest of a graph: the dependency files' digest, extended by the applicable
    /// snapshots when there are any; and those snapshots' ids.
    fn graph_digest(files_digest: &str, snaps: &[SnapshotRec]) -> (String, Vec<String>) {
        let submitted_ids: Vec<String> = snaps
            .iter()
            .map(|s| crate::ids::hex(&Sha256::digest(prost::Message::encode_to_vec(s))))
            .collect();
        if submitted_ids.is_empty() {
            return (files_digest.to_string(), submitted_ids);
        }
        let mut h = Sha256::new();
        h.update(files_digest.as_bytes());
        for s in &submitted_ids {
            h.update(s.as_bytes());
        }
        (crate::ids::hex(&h.finalize()), submitted_ids)
    }

    /// The graph of parsed `files` plus the applicable snapshots.
    fn assemble(files: &[SourceFile], snaps: &[SnapshotRec]) -> DependencyGraph {
        let parsed = DependencyGraph::from_files(files);
        let mut packages: Vec<Package> = parsed.packages().to_vec();
        let mut manifests: Vec<ManifestInfo> = parsed.manifests().to_vec();
        for s in snaps {
            let (p, m) = snapshot_graph_parts(s);
            packages.extend(p);
            manifests.extend(m);
        }
        DependencyGraph::new(packages, manifests)
    }

    /// The merged graph of a revision: parsed manifests plus the submitted snapshots that apply
    /// (those for exactly this changeset, and the newest of the default branch when `at` is
    /// the default branch tip).
    pub(crate) async fn graph_at(
        &self,
        cx: &Cx,
        repo: &Repo,
        at: &ObjectId,
    ) -> Result<(DependencyGraph, Vec<SourceFile>, String, Vec<String>)> {
        let (files, files_digest) = self.dependency_files(repo, at).await?;
        let snaps = self.applicable_snapshots(cx, repo, at).await?;
        let (digest, submitted_ids) = Self::graph_digest(&files_digest, &snaps);
        Ok((Self::assemble(&files, &snaps), files, digest, submitted_ids))
    }
    fn submitted_paths(snaps: &[SnapshotRec]) -> BTreeSet<String> {
        snaps
            .iter()
            .flat_map(|s| s.manifests.iter().map(|m| m.path.clone()))
            .collect()
    }

    /// `ListDependencies`.
    pub async fn list_dependencies(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ListDependenciesRequest,
        token: &str,
        size: usize,
    ) -> Result<ListDependenciesResponse> {
        let at = self
            .ports
            .vcs
            .resolve(
                repo,
                req.revision.as_ref().map(|r| r.expr.as_str()).unwrap_or(""),
            )
            .await?;
        let (graph, _, _, _) = self.graph_at(cx, repo, &at).await?;
        let submitted = Self::submitted_paths(&self.snapshots(cx, repo).await?);
        let eco = if req.ecosystem.is_empty() {
            None
        } else {
            Some(Ecosystem::from_proto_name(&req.ecosystem).ok_or_else(|| {
                ForgeError::invalid("unknown_ecosystem", "unknown ecosystem filter")
            })?)
        };
        let items: Vec<Dependency> = graph
            .packages()
            .iter()
            .filter(|p| req.manifest_path.is_empty() || p.source_path == req.manifest_path)
            .filter(|p| eco.is_none_or(|e| p.ecosystem == e))
            .filter(|p| {
                req.scope == 0
                    || req.scope
                        == match p.scope {
                            Scope::Runtime => DependencyScope::Runtime as i32,
                            Scope::Development => DependencyScope::Development as i32,
                        }
            })
            .map(|p| pkg_to_proto(p, submitted.contains(&p.source_path)))
            .collect();
        let (page, next) = page_slice(&items, token, size)?;
        Ok(ListDependenciesResponse {
            dependencies: page,
            changeset: Some(at),
            page: crate::service::next(next),
        })
    }

    /// `ListDependencyManifests`.
    pub async fn list_manifests(
        &self,
        cx: &Cx,
        repo: &Repo,
        revision: &str,
        token: &str,
        size: usize,
    ) -> Result<(Vec<DependencyManifest>, String)> {
        let at = self.ports.vcs.resolve(repo, revision).await?;
        let (graph, _, _, _) = self.graph_at(cx, repo, &at).await?;
        let submitted = Self::submitted_paths(&self.snapshots(cx, repo).await?);
        let items: Vec<DependencyManifest> = graph
            .manifests()
            .iter()
            .map(|m| {
                manifest_to_proto(
                    m,
                    if submitted.contains(&m.path) {
                        "submitted"
                    } else {
                        "static"
                    },
                )
            })
            .collect();
        page_slice(&items, token, size)
    }

    /// `SubmitDependencySnapshot`: a later snapshot of the same detector and job correlator
    /// replaces the earlier one; the default branch graph is re-evaluated when it applies.
    pub async fn submit_snapshot(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: SubmitDependencySnapshotRequest,
    ) -> Result<SubmitDependencySnapshotResponse> {
        if req.detector_name.is_empty() {
            return Err(ForgeError::missing("detector_name"));
        }
        if req.job_correlator.is_empty() {
            return Err(ForgeError::missing("job_correlator"));
        }
        let total: usize = req.manifests.iter().map(|m| m.dependencies.len()).sum();
        if total > MAX_SNAPSHOT_DEPENDENCIES {
            return Err(ForgeError::too_large(
                "snapshot_dependencies",
                MAX_SNAPSHOT_DEPENDENCIES as u64,
                total as u64,
            ));
        }
        for m in &req.manifests {
            if m.path.is_empty() {
                return Err(ForgeError::missing("manifests.path"));
            }
            for d in &m.dependencies {
                Purl::parse(&d.package_url)
                    .map_err(|e| ForgeError::invalid("invalid_package_url", e.message))?;
                for u in &d.dependency_urls {
                    Purl::parse(u)
                        .map_err(|e| ForgeError::invalid("invalid_package_url", e.message))?;
                }
            }
        }
        let id = format!("{}:{}", req.detector_name, req.job_correlator);
        let now = self.now();
        let rec = SnapshotRec {
            id: id.clone(),
            detector: req.detector_name.clone(),
            detector_version: req.detector_version.clone(),
            correlator: req.job_correlator.clone(),
            job_id: req.job_id.clone(),
            changeset: req
                .changeset
                .as_ref()
                .map(|c| c.raw.clone())
                .unwrap_or_default(),
            ref_name: if req.r#ref.is_empty() {
                repo.default_ref.clone()
            } else {
                req.r#ref.clone()
            },
            manifests: req.manifests.clone(),
            scanned_ms: req.scanned_at.as_ref().map(ts_ms).unwrap_or(now),
            dependency_count: total as u32,
        };
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let existed = tx.get(&SNAPSHOTS, &repo.id, &id).await?.is_some();
        let row = crate::store::Row::new(&repo.id, &id)
            .n1(now as i64)
            .msg(&rec);
        tx.put(&SNAPSHOTS, &row).await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "dependency_snapshot.submitted",
                action: "security.dependency_snapshot.submit",
                subject: SubjectKind::Repo,
                id: &repo.id,
                payload: serde_json::json!({
                    "snapshot_id": id, "detector": req.detector_name,
                    "dependency_count": total, "replaced": existed,
                }),
                before: String::new(),
                after: serde_json::json!({"snapshot_id": id, "dependency_count": total})
                    .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        // A snapshot for the default branch changes its graph.
        if rec.ref_name == repo.default_ref
            && let Ok(tip) = self.ports.vcs.resolve(repo, "").await
        {
            self.on_default_branch_moved(&Cx::system(), repo, &tip)
                .await?;
        }
        Ok(SubmitDependencySnapshotResponse {
            snapshot_id: id,
            dependency_count: total as u32,
        })
    }

    /// Re-parses the default branch at `changeset` when its dependency files (or submitted
    /// snapshots) changed, stores the graph and re-evaluates its alerts.
    ///
    /// Incremental: when a graph was stored before, only the paths that changed since its
    /// changeset are looked at (the VCS diffs the two trees at the cost of the difference);
    /// when none of them is a dependency file nothing is read. The whole tree is listed only
    /// for the first graph of a repo or when the VCS cannot diff.
    pub async fn on_default_branch_moved(
        &self,
        cx: &Cx,
        repo: &Repo,
        changeset: &ObjectId,
    ) -> Result<bool> {
        let config = self.config_of(cx, repo).await?;
        if !config.dependency_graph || repo.archived {
            return Ok(false);
        }
        self.ensure_index(cx).await?;
        let generation = self.generation();
        let old = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.get_msg::<GraphRec>(&GRAPHS, "", &repo.id)
                .await?
                .map(|(g, _)| g)
        };
        let changed = match &old {
            Some(o) if o.paths_recorded && o.changeset == changeset.raw => Some(Vec::new()),
            Some(o) if o.paths_recorded && !o.changeset.is_empty() => {
                let from = ObjectId {
                    raw: o.changeset.clone(),
                };
                // A base the VCS can no longer read falls back to a full listing.
                self.ports
                    .vcs
                    .changed_paths(repo, &from, changeset)
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        // `files` is `None` when the dependency files are known to be those of the old graph.
        let (mut files, files_digest) = match (&old, changed) {
            (Some(o), Some(changed)) => {
                let touched: Vec<String> = changed
                    .into_iter()
                    .filter(|p| is_dependency_file(p))
                    .collect();
                if touched.is_empty() {
                    (None, o.files_digest.clone())
                } else {
                    let paths: BTreeSet<String> =
                        o.dependency_paths.iter().cloned().chain(touched).collect();
                    let (f, d) = self.read_dependency_files(repo, changeset, paths).await?;
                    (Some(f), d)
                }
            }
            _ => {
                let (f, d) = self.dependency_files(repo, changeset).await?;
                (Some(f), d)
            }
        };
        let snaps = self.applicable_snapshots(cx, repo, changeset).await?;
        let (digest, _) = Self::graph_digest(&files_digest, &snaps);
        if let Some(o) = &old
            && o.digest == digest
            && (o.advisory_generation == generation || !config.dependency_alerts)
        {
            // Same graph: move the stored base forward so the next diff starts here.
            if o.changeset != changeset.raw || !o.paths_recorded {
                let mut rec = o.clone();
                rec.changeset = changeset.raw.clone();
                if let Some(f) = &files {
                    rec.dependency_paths = f.iter().map(|f| f.path.clone()).collect();
                }
                rec.files_digest = files_digest;
                rec.paths_recorded = true;
                let mut tx = self.db.begin(Surface("security"), cx).await?;
                tx.put(
                    &GRAPHS,
                    &crate::store::Row::new("", &repo.id)
                        .n1(rec.updated_ms as i64)
                        .msg(&rec),
                )
                .await?;
                tx.commit().await?;
            }
            return Ok(false);
        }
        let files = match files.take() {
            Some(f) => f,
            None => {
                let paths = old
                    .as_ref()
                    .map(|o| o.dependency_paths.clone())
                    .unwrap_or_default();
                self.read_dependency_files(repo, changeset, paths).await?.0
            }
        };
        let graph = Self::assemble(&files, &snaps);
        let submitted_paths = Self::submitted_paths(&self.snapshots(cx, repo).await?);
        let rec = GraphRec {
            changeset: changeset.raw.clone(),
            ref_name: repo.default_ref.clone(),
            digest: digest.clone(),
            packages: graph
                .packages()
                .iter()
                .map(|p| pkg_to_rec(p, submitted_paths.contains(&p.source_path)))
                .collect(),
            manifests: graph
                .manifests()
                .iter()
                .map(|m| {
                    manifest_to_proto(
                        m,
                        if submitted_paths.contains(&m.path) {
                            "submitted"
                        } else {
                            "static"
                        },
                    )
                })
                .collect(),
            published: published_names(&files),
            updated_ms: self.now(),
            advisory_generation: generation,
            dependency_paths: files.iter().map(|f| f.path.clone()).collect(),
            files_digest,
            paths_recorded: true,
        };
        let changed_files = old.as_ref().is_none_or(|o| o.digest != digest);
        {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.put(
                &GRAPHS,
                &crate::store::Row::new("", &repo.id)
                    .n1(rec.updated_ms as i64)
                    .msg(&rec),
            )
            .await?;
            if changed_files {
                record(
                    &mut tx,
                    Change {
                        repo,
                        event: "dependency_graph.updated",
                        action: "security.dependency_graph.update",
                        subject: SubjectKind::Repo,
                        id: &repo.id,
                        payload: serde_json::json!({
                            "changeset": hex_id(changeset),
                            "packages": rec.packages.len(),
                            "manifests": rec.manifests.len(),
                        }),
                        before: String::new(),
                        after: serde_json::json!({
                            "changeset": hex_id(changeset),
                            "packages": rec.packages.len(),
                        })
                        .to_string(),
                    },
                )
                .await?;
            }
            tx.commit().await?;
        }
        if config.dependency_alerts {
            self.evaluate_graph(cx, repo, &graph).await?;
        }
        Ok(true)
    }

    /// Re-parses the default branch even when its files did not change (used when the graph or
    /// alerts are switched on).
    pub(crate) async fn force_graph_evaluation(
        &self,
        cx: &Cx,
        repo: &Repo,
        changeset: &ObjectId,
    ) -> Result<bool> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.delete(&GRAPHS, "", &repo.id).await?;
        tx.commit().await?;
        self.on_default_branch_moved(cx, repo, changeset).await
    }

    /// `ListDependents`: repos (public, or in the same org) whose graph holds a package this
    /// repo publishes.
    pub async fn list_dependents(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ListDependentsRequest,
        token: &str,
        size: usize,
    ) -> Result<(Vec<DependencyDependent>, u64, String)> {
        let mine = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.get_msg::<GraphRec>(&GRAPHS, "", &repo.id)
                .await?
                .map(|(g, _)| g.published)
                .unwrap_or_default()
        };
        let wanted: BTreeSet<(Ecosystem, String)> = if req.package_url.is_empty() {
            mine.iter()
                .filter_map(|n| {
                    let (e, name) = n.split_once(':')?;
                    Some((Ecosystem::from_proto_name(e)?, name.to_string()))
                })
                .collect()
        } else {
            let p = Purl::parse(&req.package_url)
                .map_err(|e| ForgeError::invalid("invalid_package_url", e.message))?;
            let eco = Ecosystem::from_purl_type(&p.ty).ok_or_else(|| {
                ForgeError::invalid("invalid_package_url", "unsupported package type")
            })?;
            let mut name = p.namespace.join("/");
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str(&p.name);
            let mine_names: BTreeSet<String> = mine.iter().cloned().collect();
            if !mine_names.contains(&format!("{}:{}", eco.proto_name(), name)) {
                return Err(ForgeError::not_found(format!(
                    "package {} published by this repo",
                    req.package_url
                )));
            }
            BTreeSet::from([(eco, name)])
        };
        let rows = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<GraphRec>(&GRAPHS, &Scan::scope("", crate::store::MAX_SCAN))
                .await?
        };
        let mut out: Vec<DependencyDependent> = Vec::new();
        for (g, row) in rows {
            if row.id == repo.id {
                continue;
            }
            let hits: Vec<&PkgRec> = g
                .packages
                .iter()
                .filter(|p| {
                    Ecosystem::from_proto_name(&p.ecosystem)
                        .is_some_and(|e| wanted.contains(&(e, p.name.clone())))
                })
                .collect();
            if hits.is_empty() {
                continue;
            }
            let Ok(other) = self.orgs.repo_any(cx, &row.id).await else {
                continue;
            };
            let visible = other.org_id == repo.org_id
                || other.visibility == bee_proto::v1::Visibility::Public as i32;
            if !visible || other.deleted_at.is_some() {
                continue;
            }
            for p in hits {
                out.push(DependencyDependent {
                    repo: repo_ref_of(&other),
                    package_url: p.purl.clone(),
                    version: p.version.clone(),
                });
            }
        }
        out.sort_by(|a, b| {
            (a.package_url.clone(), format!("{:?}", a.repo))
                .cmp(&(b.package_url.clone(), format!("{:?}", b.repo)))
        });
        let total = out.len() as u64;
        let (page, next) = page_slice(&out, token, size)?;
        Ok((page, total, next))
    }

    /// `ExportSbom`.
    pub async fn export_sbom(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ExportSbomRequest,
    ) -> Result<ExportSbomResponse> {
        let format = SbomFormat::try_from(req.format).unwrap_or(SbomFormat::Unspecified);
        if format == SbomFormat::Unspecified {
            return Err(ForgeError::invalid("missing_field", "`format` is required"));
        }
        let at = self
            .ports
            .vcs
            .resolve(
                repo,
                req.revision.as_ref().map(|r| r.expr.as_str()).unwrap_or(""),
            )
            .await?;
        let (graph, _, _, _) = self.graph_at(cx, repo, &at).await?;
        let mut opts = SbomOptions::new(&repo.full_name, &super::rfc3339(self.now()));
        if let Ok(base) = self.sbom_namespace_base.read()
            && let Some(base) = base.as_ref()
        {
            opts.namespace_base = base.clone();
        }
        let (doc, ctype, ext) = match format {
            SbomFormat::SpdxJson => (
                spdx_json(&graph, &opts),
                "application/spdx+json",
                "spdx.json",
            ),
            _ => (
                cyclonedx_json(&graph, &opts),
                "application/vnd.cyclonedx+json",
                "cdx.json",
            ),
        };
        let text = to_json_string(&doc);
        let wants_url = req.prefer_signed_url || text.len() > SIGNED_URL_THRESHOLD;
        if wants_url && let Some(p) = &self.ports.publisher {
            let name = format!(
                "{}-{}.{ext}",
                repo.name,
                &hex_id(&at)[..12.min(hex_id(&at).len())]
            );
            let (url, expires) = p.publish(&name, ctype, text.into_bytes()).await?;
            return Ok(ExportSbomResponse {
                format: format as i32,
                body: Some(bee_proto::v1::export_sbom_response::Body::SignedUrl(url)),
                url_expires_at: Some(crate::ids::ts(expires)),
                changeset: Some(at),
            });
        }
        if text.len() > MAX_INLINE_SBOM {
            return Err(ForgeError::too_large(
                "sbom_bytes",
                MAX_INLINE_SBOM as u64,
                text.len() as u64,
            ));
        }
        Ok(ExportSbomResponse {
            format: format as i32,
            body: Some(bee_proto::v1::export_sbom_response::Body::Document(text)),
            url_expires_at: None,
            changeset: Some(at),
        })
    }

    /// `GetDependencyReview`.
    pub async fn dependency_review(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &GetDependencyReviewRequest,
    ) -> Result<ProtoReview> {
        use bee_proto::v1::get_dependency_review_request::Target;
        let (base, head, from_cr) = match &req.target {
            Some(Target::ChangeRequest(n)) => {
                let changes = self.changes.as_ref().ok_or_else(|| {
                    crate::service::unserved("GetDependencyReview by change request", "changes")
                })?;
                let number: u64 = n.trim_start_matches('#').parse().map_err(|_| {
                    ForgeError::invalid("bad_change_request", "change request number expected")
                })?;
                let cr = changes.get_by_number(cx, repo, number).await?;
                let rev = cr
                    .revisions
                    .last()
                    .ok_or_else(|| ForgeError::invalid_state("change request has no revision"))?;
                let base = rev
                    .base
                    .clone()
                    .ok_or_else(|| ForgeError::invalid_state("revision has no base"))?;
                let head = rev
                    .head
                    .clone()
                    .ok_or_else(|| ForgeError::invalid_state("revision has no head"))?;
                (base, head, true)
            }
            Some(Target::Range(DependencyReviewRange { base, head })) => {
                let b = self
                    .ports
                    .vcs
                    .resolve(repo, base.as_ref().map(|r| r.expr.as_str()).unwrap_or(""))
                    .await?;
                let h = self
                    .ports
                    .vcs
                    .resolve(repo, head.as_ref().map(|r| r.expr.as_str()).unwrap_or(""))
                    .await?;
                (b, h, false)
            }
            None => return Err(ForgeError::missing("target")),
        };
        self.ensure_index(cx).await?;
        let (bg, _, _, _) = self.graph_at(cx, repo, &base).await?;
        let (hg, _, _, _) = self.graph_at(cx, repo, &head).await?;
        let fail_on = match SecuritySeverity::try_from(req.fail_on_severity).unwrap_or_default() {
            SecuritySeverity::Unspecified => Some(bee_deps::cvss::Severity::Low),
            SecuritySeverity::Low => Some(bee_deps::cvss::Severity::Low),
            SecuritySeverity::Medium => Some(bee_deps::cvss::Severity::Medium),
            SecuritySeverity::High => Some(bee_deps::cvss::Severity::High),
            SecuritySeverity::Critical => Some(bee_deps::cvss::Severity::Critical),
        };
        let result = {
            let idx = self.index.read().map_err(super::advisories::poisoned)?;
            review(
                &bg,
                &hg,
                &idx.source,
                &ReviewPolicy {
                    fail_on_severity: fail_on,
                    ..ReviewPolicy::default()
                },
                &MatchOptions::default(),
            )
        };
        let changes: Vec<DependencyChange> = result
            .changes
            .iter()
            .map(|c| DependencyChange {
                change_type: match c.change_type {
                    ChangeType::Added => DependencyChangeType::Added,
                    ChangeType::Removed => DependencyChangeType::Removed,
                    ChangeType::Changed => DependencyChangeType::Changed,
                } as i32,
                manifest_path: c.manifest_path.clone(),
                ecosystem: c.ecosystem.proto_name().to_string(),
                name: c.name.clone(),
                base_version: c.base_version.clone().unwrap_or_default(),
                head_version: c.head_version.clone().unwrap_or_default(),
                package_url: c.purl.clone(),
                licenses: c.licenses.clone(),
                scope: match c.scope {
                    Scope::Runtime => DependencyScope::Runtime,
                    Scope::Development => DependencyScope::Development,
                } as i32,
                vulnerabilities: c
                    .vulnerabilities
                    .iter()
                    .filter_map(|f| self.advisory_proto(&f.advisory_id))
                    .collect(),
                license_denied: c.license_denied,
            })
            .collect();
        let mut check_run_id = String::new();
        if from_cr {
            let existing = self
                .checks
                .list(
                    cx,
                    &repo.id,
                    &RunFilter {
                        changeset_hex: Some(hex_id(&head)),
                        name: "dependency-review".into(),
                        ..RunFilter::default()
                    },
                )
                .await?;
            let status = if result.passes {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
            let cr_result = result.to_check_result();
            match existing.first() {
                Some(run) if run.status == status as i32 => check_run_id = run.id.clone(),
                _ => {
                    let mut sys = Cx::system();
                    sys.request_id = cx.request_id.clone();
                    sys.decision = "security:dependency-review".into();
                    let run = self
                        .checks
                        .report(
                            &sys,
                            &repo.org_id,
                            &repo.id,
                            Report {
                                changeset: Some(head.clone()),
                                name: "dependency-review".into(),
                                status,
                                summary: cr_result.title.clone(),
                                output: Some(CheckOutput {
                                    title: cr_result.title,
                                    summary: cr_result.summary,
                                    ..Default::default()
                                }),
                                ..Report::default()
                            },
                        )
                        .await?;
                    check_run_id = run.id;
                }
            }
        }
        Ok(ProtoReview {
            base: Some(base),
            head: Some(head),
            changes,
            vulnerable_additions: result.vulnerable_additions,
            highest_severity: sev(result.highest_severity) as i32,
            passes: result.passes,
            check_run_id,
        })
    }

    /// Findings of a graph against the current index (used by the update planner).
    pub(crate) fn findings_of(
        &self,
        graph: &DependencyGraph,
    ) -> Result<Vec<bee_deps::matching::Finding>> {
        let idx = self.index.read().map_err(super::advisories::poisoned)?;
        Ok(match_graph(graph, &idx.source, &MatchOptions::default()))
    }
}
