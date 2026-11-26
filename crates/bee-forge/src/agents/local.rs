//! [`LocalProvider`]: workspaces as working-copy directories on the local disk, for tests and
//! `bee-server --dev`. No container runs; the devcontainer spec, machine and egress policy
//! are written next to the working copy so tools (and tests) can inspect them.
//!
//! Layout under the root: `<workspace id>/wc/` (the working copy), `<workspace id>/base.json`
//! (path → BLAKE3 of the base content, for computing edits), `<workspace id>/container.json`,
//! `<workspace id>/state`, and `prebuilds/<prebuild id>/` (a materialized changeset that new
//! workspaces copy instead of fetching from the repo).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{ObjectId, Workspace};

use super::ports::{
    AgentRepo, BuildRequest, Endpoints, Provisioned, WorkspaceEdits, WorkspaceProvider,
};
use crate::error::{ForgeError, Result};
use crate::ids::hex;

/// Most files materialized into one working copy.
pub const MAX_FILES: usize = 100_000;

/// Workspaces as directories.
pub struct LocalProvider {
    root: PathBuf,
    repo: Arc<dyn AgentRepo>,
}

fn io(e: std::io::Error) -> ForgeError {
    ForgeError::unavailable(format!("local workspace: {e}"))
}

fn safe_rel(p: &str) -> Result<&str> {
    if p.is_empty()
        || p.starts_with('/')
        || p.split('/').any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(ForgeError::invalid(
            "invalid_path",
            format!("unsafe path {p:?}"),
        ));
    }
    Ok(p)
}

fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(io)?
        .collect::<std::result::Result<_, _>>()
        .map_err(io)?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let ft = e.file_type().map_err(io)?;
        if ft.is_dir() {
            walk(&e.path(), &rel, out)?;
        } else if ft.is_file() {
            out.insert(rel, std::fs::read(e.path()).map_err(io)?);
        }
    }
    Ok(())
}

fn digest(b: &[u8]) -> String {
    hex(blake3::hash(b).as_bytes())
}

impl LocalProvider {
    /// A provider keeping workspaces under `root`, reading repo content through `repo`.
    pub fn new(root: impl Into<PathBuf>, repo: Arc<dyn AgentRepo>) -> LocalProvider {
        LocalProvider {
            root: root.into(),
            repo,
        }
    }

    /// The working copy directory of a workspace handle.
    pub fn working_copy(&self, handle: &str) -> PathBuf {
        self.root.join(handle).join("wc")
    }

    fn dir(&self, handle: &str) -> Result<PathBuf> {
        if handle.is_empty() || handle.contains('/') || handle.contains("..") {
            return Err(ForgeError::invalid(
                "invalid_handle",
                "bad workspace handle",
            ));
        }
        Ok(self.root.join(handle))
    }

    async fn materialize(
        &self,
        req: &BuildRequest,
        at: &ObjectId,
        into: &Path,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut files = BTreeMap::new();
        let prefixes: Vec<String> = if req.workspace.sparse_paths.is_empty() {
            vec![String::new()]
        } else {
            req.workspace
                .sparse_paths
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect()
        };
        for prefix in prefixes {
            let (list, _) = self
                .repo
                .list_files(&req.repo, at, &prefix, MAX_FILES)
                .await?;
            for f in list {
                if let Some((_, bytes)) = self.repo.read_file(&req.repo, at, &f.path).await? {
                    files.insert(f.path, bytes);
                }
            }
        }
        write_tree(into, &files)?;
        Ok(files)
    }

    fn write_meta(&self, dir: &Path, req: &BuildRequest) -> Result<()> {
        let meta = serde_json::json!({
            "spec": serde_json::from_str::<serde_json::Value>(&req.spec.to_json()).unwrap_or_default(),
            "machine": req.machine.name,
            "cpus": req.machine.cpus,
            "egress": serde_json::from_str::<serde_json::Value>(&super::pjson(&req.egress)).unwrap_or_default(),
            "dotfiles": req.dotfiles.as_ref().map(|(r, c)| serde_json::json!({"repo": r, "install": c})),
            "prebuild": req.prebuild_handle,
        });
        std::fs::write(dir.join("container.json"), meta.to_string()).map_err(io)
    }

    fn write_manifest(dir: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
        let m: BTreeMap<&String, String> = files.iter().map(|(k, v)| (k, digest(v))).collect();
        std::fs::write(
            dir.join("base.json"),
            serde_json::to_string(&m).unwrap_or_default(),
        )
        .map_err(io)
    }

    fn ports(req: &BuildRequest) -> Vec<(u32, String, String)> {
        req.spec
            .forward_ports
            .iter()
            .map(|p| (p.port, p.label.clone(), p.protocol.clone()))
            .collect()
    }
}

fn write_tree(into: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    std::fs::create_dir_all(into).map_err(io)?;
    for (path, bytes) in files {
        let rel = safe_rel(path)?;
        let p = into.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        std::fs::write(&p, bytes).map_err(io)?;
    }
    Ok(())
}

#[async_trait]
impl WorkspaceProvider for LocalProvider {
    async fn provision(&self, req: &BuildRequest) -> Result<Provisioned> {
        let handle = req.workspace.id.clone();
        let dir = self.dir(&handle)?;
        let wc = dir.join("wc");
        std::fs::create_dir_all(&wc).map_err(io)?;
        let pre = Path::new(&req.prebuild_handle);
        let files = if !req.prebuild_handle.is_empty() && pre.is_dir() {
            let mut files = BTreeMap::new();
            walk(pre, "", &mut files)?;
            write_tree(&wc, &files)?;
            files
        } else {
            let at = req.workspace.base_changeset.clone().unwrap_or_default();
            self.materialize(req, &at, &wc).await?
        };
        Self::write_manifest(&dir, &files)?;
        self.write_meta(&dir, req)?;
        std::fs::write(dir.join("state"), "running").map_err(io)?;
        Ok(Provisioned {
            handle,
            ports: Self::ports(req),
        })
    }

    async fn start(&self, handle: &str, req: &BuildRequest) -> Result<Provisioned> {
        let dir = self.dir(handle)?;
        if !dir.exists() {
            return Err(ForgeError::not_found(format!("workspace disk {handle}")));
        }
        self.write_meta(&dir, req)?;
        std::fs::write(dir.join("state"), "running").map_err(io)?;
        Ok(Provisioned {
            handle: handle.to_string(),
            ports: Self::ports(req),
        })
    }

    async fn stop(&self, handle: &str) -> Result<()> {
        let dir = self.dir(handle)?;
        std::fs::write(dir.join("state"), "stopped").map_err(io)
    }

    async fn rebuild(&self, handle: &str, req: &BuildRequest, full: bool) -> Result<Provisioned> {
        let dir = self.dir(handle)?;
        self.write_meta(&dir, req)?;
        std::fs::write(
            dir.join("state"),
            if full {
                "running (full rebuild)"
            } else {
                "running"
            },
        )
        .map_err(io)?;
        Ok(Provisioned {
            handle: handle.to_string(),
            ports: Self::ports(req),
        })
    }

    async fn destroy(&self, handle: &str) -> Result<()> {
        let dir = self.dir(handle)?;
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(io)?;
        }
        Ok(())
    }

    async fn edits(&self, handle: &str) -> Result<WorkspaceEdits> {
        if handle.is_empty() {
            return Ok(WorkspaceEdits::default());
        }
        let dir = self.dir(handle)?;
        let base: BTreeMap<String, String> = match std::fs::read_to_string(dir.join("base.json")) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => return Ok(WorkspaceEdits::default()),
        };
        let mut now = BTreeMap::new();
        walk(&dir.join("wc"), "", &mut now)?;
        let mut edits = Vec::new();
        for (p, b) in &now {
            if base.get(p) != Some(&digest(b)) {
                edits.push((p.clone(), Some(b.clone())));
            }
        }
        for p in base.keys() {
            if !now.contains_key(p) {
                edits.push((p.clone(), None));
            }
        }
        edits.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(WorkspaceEdits { edits })
    }

    async fn rebase(&self, handle: &str) -> Result<()> {
        let dir = self.dir(handle)?;
        let mut now = BTreeMap::new();
        walk(&dir.join("wc"), "", &mut now)?;
        Self::write_manifest(&dir, &now)
    }

    async fn endpoints(&self, handle: &str, w: &Workspace) -> Result<Endpoints> {
        let wc = self.dir(handle)?.join("wc");
        Ok(Endpoints {
            web_ide_url: format!("file://{}", wc.display()),
            ssh_host: "localhost".into(),
            ssh_port: 22,
            ssh_user: w.owner.as_ref().map(|o| o.id.clone()).unwrap_or_default(),
        })
    }

    async fn prebuild(&self, req: &BuildRequest, changeset: &ObjectId) -> Result<String> {
        let id = &req.workspace.id;
        if id.is_empty() || id.contains('/') {
            return Err(ForgeError::invalid("invalid_handle", "bad prebuild id"));
        }
        let dir = self.root.join("prebuilds").join(id);
        self.materialize(req, changeset, &dir).await?;
        Ok(dir.display().to_string())
    }

    async fn delete_prebuild(&self, handle: &str) -> Result<()> {
        let p = Path::new(handle);
        if p.starts_with(self.root.join("prebuilds")) && p.exists() {
            std::fs::remove_dir_all(p).map_err(io)?;
        }
        Ok(())
    }
}
