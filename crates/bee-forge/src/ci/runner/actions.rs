//! Actions the local runner can run.
//!
//! Built in: `actions/checkout` (the job's repo through the API), `actions/setup-*` (verify
//! the tool is on `PATH`; versions are not downloaded), `actions/upload-artifact`,
//! `actions/download-artifact`, `actions/cache` (plus `cache/restore` and `cache/save`).
//!
//! Other actions are local (`uses: ./path`) or fetched once per job from another repository
//! (`uses: owner/repo[/path]@ref`) through the runner's [`super::registry`]:
//!
//! - **composite** (`runs.using: composite`): the steps run in the job with the action's
//!   `inputs`, `github.action_path` and `GITHUB_ACTION_PATH` set;
//! - **JavaScript** (`runs.using: node*`), when `node` is on `PATH`: `runs.main` runs with the
//!   inputs as `INPUT_<NAME>` and the `GITHUB_OUTPUT`/`GITHUB_ENV`/`GITHUB_PATH`/
//!   `GITHUB_STATE`/`GITHUB_STEP_SUMMARY` file protocol; `runs.pre` runs at job start and
//!   `runs.post` at job end (`pre-if`/`post-if`, default `always()`), state saved in
//!   `GITHUB_STATE` reaching later scripts as `STATE_<name>`;
//! - **Docker** actions are reported as unsupported by the local runner with an explicit
//!   error (the local runner does not use Docker).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bee_workflow::Value;
use bee_workflow::expr::Template;
use bee_workflow::value::Object;
use bee_workflow::yaml::Node;

use super::exec::{ActionInfo, Job, Scope, StepKind, StepOutcome, StepSpec, walk};
use super::registry::ActionRef;
use crate::ci::util::{sha256_digest, sha256_hex, unzip, zip};
use crate::error::{ForgeError, Result};
use bee_workflow::expr::Condition;

/// Work an action schedules for the end of the job.
#[derive(Clone, Debug)]
pub enum PostAction {
    /// Save a cache entry unless the key was restored exactly.
    CacheSave {
        /// Primary key.
        key: String,
        /// Version (hash of the paths).
        version: String,
        /// Paths to save.
        paths: Vec<String>,
    },
    /// Run a JavaScript action's `post` script.
    Node {
        /// The `uses:` reference.
        label: String,
        /// The action.
        info: ActionInfo,
        /// The script.
        script: PathBuf,
        /// Step environment with the inputs.
        env: BTreeMap<String, String>,
        /// State saved by `pre` and `main`.
        state: BTreeMap<String, String>,
        /// `post-if`.
        condition: String,
    },
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn unsupported(action: &str, why: &str) -> ForgeError {
    ForgeError::invalid(
        "unsupported_in_local_runner",
        format!(
            "{action}: {why} are not supported by the local runner; run this job on a runner with a full actions runtime"
        ),
    )
}

fn base_name(action: &str) -> (String, String) {
    let (name, r) = action.split_once('@').unwrap_or((action, ""));
    (name.to_lowercase(), r.to_string())
}

/// Runs one `uses:` step.
pub async fn run_action(
    job: &mut Job,
    action: &str,
    with: BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
    scope: &mut Scope,
    timeout: Option<std::time::Duration>,
) -> Result<StepOutcome> {
    if let Some(img) = action.strip_prefix("docker://") {
        return Err(unsupported(img, "Docker actions"));
    }
    if action.starts_with("./") {
        return local_action(job, action, with, env, timeout).await;
    }
    let (name, _) = base_name(action);
    let _ = scope;
    match name.as_str() {
        "actions/checkout" => checkout(job, &with).await,
        "actions/upload-artifact" => upload_artifact(job, &with).await,
        "actions/download-artifact" => download_artifact(job, &with).await,
        "actions/cache" => cache(job, &with, true, true).await,
        "actions/cache/restore" => cache(job, &with, true, false).await,
        "actions/cache/save" => cache(job, &with, false, true).await,
        n if n.starts_with("actions/setup-") => {
            setup(job, n.trim_start_matches("actions/setup-"), &with).await
        }
        _ => remote_action(job, action, with, env, timeout).await,
    }
}

/// Runs scheduled post work: cache saves after a successful job, `post` scripts when their
/// `post-if` holds (a failing `post` fails the job).
pub async fn run_post(job: &mut Job, p: PostAction) -> Result<()> {
    match p {
        PostAction::Node {
            label,
            info,
            script,
            env,
            state,
            condition,
        } => {
            let ctx = job.ctx(&Scope::default(), &BTreeMap::new());
            let run = Condition::parse(&condition)
                .ok()
                .and_then(|c| c.evaluate(&ctx).ok())
                .unwrap_or(false);
            if !run {
                return Ok(());
            }
            job.line(&format!("##[group]Post {label}")).await?;
            job.line("##[endgroup]").await?;
            let mut env = env;
            env.extend(state_env(&state));
            let timeout = Some(job.remaining_timeout());
            let outcome = run_node(job, &info, &script, &env, timeout).await?;
            if !outcome.ok {
                job.failed = true;
            }
            Ok(())
        }
        PostAction::CacheSave { .. }
            if job.failed || job.cancel.load(std::sync::atomic::Ordering::SeqCst) =>
        {
            Ok(())
        }
        PostAction::CacheSave {
            key,
            version,
            paths,
        } => {
            job.line(&format!("Post cache: saving {key}")).await?;
            let bytes = pack(&job.workspace, &paths)?;
            let a = job.a.clone();
            match job.api.cache_save(&a, &key, &version, bytes).await {
                Ok(()) => job.line(&format!("Cache saved with key: {key}")).await,
                Err(e) if e.reason == bee_proto::v1::ErrorReason::AlreadyExists => {
                    job.line(&format!("Cache {key} already exists; not saving"))
                        .await
                }
                Err(e) => Err(e),
            }
        }
    }
}

fn safe_join(workspace: &Path, base: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.trim_start_matches("./");
    let normalized = rel.replace('\\', "/");
    if normalized.split('/').any(|c| c == "..")
        || Path::new(rel).is_absolute()
        || rel.starts_with('\\')
        || rel.as_bytes().get(1) == Some(&b':')
    {
        return Err(ForgeError::invalid(
            "invalid_path",
            format!("{rel} leaves the workspace"),
        ));
    }
    let candidate = if rel.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    };
    let root = std::fs::canonicalize(workspace).map_err(|e| {
        ForgeError::invalid("invalid_path", format!("cannot resolve workspace: {e}"))
    })?;
    let mut ancestor = candidate.as_path();
    let mut suffix = Vec::new();
    let resolved = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(path) => break path,
            Err(_) => {
                let Some(name) = ancestor.file_name() else {
                    return Err(ForgeError::invalid(
                        "invalid_path",
                        format!("{rel} cannot be resolved inside the workspace"),
                    ));
                };
                suffix.push(name.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    ForgeError::invalid(
                        "invalid_path",
                        format!("{rel} cannot be resolved inside the workspace"),
                    )
                })?;
            }
        }
    };
    let mut resolved = resolved;
    for part in suffix.iter().rev() {
        resolved.push(part);
    }
    if !resolved.starts_with(&root) {
        return Err(ForgeError::invalid(
            "invalid_path",
            format!("{rel} resolves outside the workspace"),
        ));
    }
    Ok(resolved)
}

async fn checkout(job: &mut Job, with: &BTreeMap<String, String>) -> Result<StepOutcome> {
    let repo = with.get("repository").cloned().unwrap_or_default();
    let own = job.plan["github"]["repository"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if !repo.is_empty() && !repo.eq_ignore_ascii_case(&own) {
        return Err(unsupported(
            "actions/checkout",
            "checkouts of other repositories",
        ));
    }
    let at = match with.get("ref").filter(|r| !r.is_empty()) {
        Some(r) => crate::ci::engine::object_id(r)
            .filter(|o| o.raw.len() >= 16)
            .ok_or_else(|| {
                ForgeError::invalid(
                    "invalid_ref",
                    format!("the local runner checks out changesets by id; `ref: {r}` is not one"),
                )
            })
            .map(Some)?,
        None => None,
    };
    let dest = safe_join(
        &job.workspace,
        &job.workspace,
        with.get("path").map(String::as_str).unwrap_or(""),
    )?;
    let a = job.a.clone();
    let files = job.api.checkout(&a, at).await?;
    let n = files.len();
    for f in files {
        let p = safe_join(&job.workspace, &dest, &f.path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ForgeError::internal(format!("checkout: {e}")))?;
        }
        std::fs::write(&p, &f.bytes).map_err(|e| ForgeError::internal(format!("checkout: {e}")))?;
        #[cfg(unix)]
        if f.executable {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
        }
    }
    job.line(&format!(
        "Checked out {own} ({n} files) into {}",
        dest.display()
    ))
    .await?;
    let sha = job
        .a
        .head_changeset
        .as_ref()
        .map(|c| crate::ids::hex(&c.raw))
        .unwrap_or_default();
    Ok(StepOutcome::success(BTreeMap::from([
        ("ref".into(), job.a.head_ref.clone()),
        ("commit".into(), sha),
    ])))
}

fn find_on_path(job: &Job, names: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = job.path_add.iter().map(PathBuf::from).collect();
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    for d in dirs {
        for n in names {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

async fn setup(job: &mut Job, tool: &str, with: &BTreeMap<String, String>) -> Result<StepOutcome> {
    let names: &[&str] = match tool {
        "node" => &["node"],
        "python" => &["python3", "python"],
        "go" => &["go"],
        "java" => &["java"],
        "dotnet" => &["dotnet"],
        "ruby" => &["ruby"],
        "rust" | "rust-toolchain" => &["cargo", "rustc"],
        "deno" => &["deno"],
        "bun" => &["bun"],
        _ => {
            return Err(unsupported(
                &format!("actions/setup-{tool}"),
                "this setup action",
            ));
        }
    };
    let version = with
        .iter()
        .find(|(k, _)| k.ends_with("-version"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    match find_on_path(job, names) {
        Some(p) => {
            job.line(&format!(
                "setup-{tool}: using {} from PATH{}",
                p.display(),
                if version.is_empty() {
                    String::new()
                } else {
                    format!(" (requested {version}; the local runner does not install versions)")
                }
            ))
            .await?;
            let key = match tool {
                "python" => "python-path",
                "node" => "node-path",
                _ => "path",
            };
            Ok(StepOutcome::success(BTreeMap::from([(
                key.to_string(),
                p.to_string_lossy().into_owned(),
            )])))
        }
        None => {
            job.line(&format!(
                "##[error]setup-{tool}: none of {} is on PATH; the local runner verifies tools but does not download them",
                names.join(", ")
            ))
            .await?;
            Ok(StepOutcome::failure())
        }
    }
}

/// Packs workspace paths (files, directories, `*` in the last component) into a zip with
/// workspace-relative names.
fn pack(workspace: &Path, paths: &[String]) -> Result<Vec<u8>> {
    let mut entries = Vec::new();
    for p in paths {
        let (dir, pat) = match p.rsplit_once('/') {
            Some((d, last)) if last.contains('*') => (d.to_string(), Some(last.to_string())),
            None if p.contains('*') => (String::new(), Some(p.clone())),
            _ => (p.clone(), None),
        };
        let base = safe_join(workspace, workspace, &dir)?;
        let mut files = Vec::new();
        match &pat {
            Some(pat) => {
                let pl = bee_workflow::glob::PatternList::new(std::slice::from_ref(pat));
                if let Ok(rd) = std::fs::read_dir(&base) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if pl.matches(&name) {
                            if e.path().is_dir() {
                                walk(&e.path(), &mut files);
                            } else {
                                files.push(e.path());
                            }
                        }
                    }
                }
            }
            None if base.is_dir() => walk(&base, &mut files),
            None if base.is_file() => files.push(base.clone()),
            None => {}
        }
        for f in files {
            let rel = f
                .strip_prefix(workspace)
                .unwrap_or(&f)
                .to_string_lossy()
                .replace('\\', "/");
            if !entries.iter().any(|(n, _): &(String, Vec<u8>)| *n == rel) {
                let safe_file = safe_join(workspace, workspace, &rel)?;
                let bytes = std::fs::read(&safe_file).map_err(|e| {
                    ForgeError::invalid(
                        "invalid_path",
                        format!("cannot read {rel} inside the workspace: {e}"),
                    )
                })?;
                entries.push((rel, bytes));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(zip(&entries))
}

fn unpack(workspace: &Path, dest: &Path, bytes: &[u8]) -> Result<usize> {
    let entries = unzip(bytes).ok_or_else(|| {
        ForgeError::new(
            bee_proto::v1::ErrorReason::Integrity,
            "not an archive the local runner wrote",
        )
    })?;
    let n = entries.len();
    for (name, data) in entries {
        let p = safe_join(workspace, dest, &name)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ForgeError::internal(format!("unpack: {e}")))?;
        }
        std::fs::write(&p, data).map_err(|e| ForgeError::internal(format!("unpack: {e}")))?;
    }
    Ok(n)
}

async fn upload_artifact(job: &mut Job, with: &BTreeMap<String, String>) -> Result<StepOutcome> {
    let name = with
        .get("name")
        .filter(|n| !n.is_empty())
        .cloned()
        .unwrap_or_else(|| "artifact".into());
    let paths = lines(with.get("path").map(String::as_str).unwrap_or(""));
    if paths.is_empty() {
        return Err(ForgeError::missing("with.path"));
    }
    let bytes = pack(&job.workspace, &paths)?;
    let count = unzip(&bytes).map(|e| e.len()).unwrap_or(0);
    if count == 0 {
        match with
            .get("if-no-files-found")
            .map(String::as_str)
            .unwrap_or("warn")
        {
            "error" => {
                job.line(&format!(
                    "##[error]No files were found with the provided path: {}",
                    paths.join(", ")
                ))
                .await?;
                return Ok(StepOutcome::failure());
            }
            "ignore" => return Ok(StepOutcome::success(BTreeMap::new())),
            _ => {
                job.line(&format!("##[warning]No files were found with the provided path: {}. No artifacts will be uploaded.", paths.join(", "))).await?;
                return Ok(StepOutcome::success(BTreeMap::new()));
            }
        }
    }
    let retention = with
        .get("retention-days")
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    let a = job.a.clone();
    let (id, url) = job.api.create_artifact(&a, &name, retention).await?;
    let digest = sha256_digest(&bytes);
    let size = bytes.len() as u64;
    job.api.upload(&a, &url, bytes).await?;
    job.api.finalize_artifact(&a, &id, size, &digest).await?;
    job.line(&format!(
        "Uploaded artifact {name} ({count} files, {size} bytes)"
    ))
    .await?;
    Ok(StepOutcome::success(BTreeMap::from([
        ("artifact-id".into(), id),
        (
            "artifact-digest".into(),
            digest.trim_start_matches("sha256:").to_string(),
        ),
    ])))
}

async fn download_artifact(job: &mut Job, with: &BTreeMap<String, String>) -> Result<StepOutcome> {
    let name = with.get("name").cloned().unwrap_or_default();
    let dest = safe_join(
        &job.workspace,
        &job.workspace,
        with.get("path").map(String::as_str).unwrap_or(""),
    )?;
    let a = job.a.clone();
    let all = job.api.list_artifacts(&a).await?;
    let chosen: Vec<_> = all
        .into_iter()
        .filter(|x| x.finalized && !x.expired && (name.is_empty() || x.name == name))
        .collect();
    if chosen.is_empty() {
        job.line(&format!(
            "##[error]Unable to find any artifacts{}",
            if name.is_empty() {
                String::new()
            } else {
                format!(" named {name}")
            }
        ))
        .await?;
        return Ok(StepOutcome::failure());
    }
    let single = !name.is_empty();
    for art in chosen {
        let bytes = job.api.download_artifact(&a, &art.id).await?;
        let target = if single {
            dest.clone()
        } else {
            dest.join(&art.name)
        };
        let n = unpack(&job.workspace, &target, &bytes)?;
        job.line(&format!(
            "Downloaded artifact {} ({n} files) to {}",
            art.name,
            target.display()
        ))
        .await?;
    }
    Ok(StepOutcome::success(BTreeMap::from([(
        "download-path".into(),
        dest.to_string_lossy().into_owned(),
    )])))
}

async fn cache(
    job: &mut Job,
    with: &BTreeMap<String, String>,
    restore: bool,
    save: bool,
) -> Result<StepOutcome> {
    let key = with.get("key").cloned().unwrap_or_default();
    if key.is_empty() {
        return Err(ForgeError::missing("with.key"));
    }
    let paths = lines(with.get("path").map(String::as_str).unwrap_or(""));
    if paths.is_empty() {
        return Err(ForgeError::missing("with.path"));
    }
    let version = sha256_hex(&format!("{}|zip", paths.join("\n")));
    let mut outputs = BTreeMap::new();
    let mut exact = false;
    if restore {
        let restore_keys = lines(with.get("restore-keys").map(String::as_str).unwrap_or(""));
        let a = job.a.clone();
        match job
            .api
            .cache_lookup(&a, &key, &version, &restore_keys)
            .await?
        {
            Some((c, is_exact, bytes)) => {
                let n = unpack(&job.workspace, &job.workspace, &bytes)?;
                exact = is_exact;
                job.line(&format!("Cache restored from key: {} ({n} files)", c.key))
                    .await?;
                outputs.insert("cache-hit".into(), is_exact.to_string());
                outputs.insert("cache-matched-key".into(), c.key);
            }
            None => {
                job.line(&format!("Cache not found for input keys: {key}"))
                    .await?;
                outputs.insert("cache-hit".into(), "false".into());
            }
        }
        outputs.insert("cache-primary-key".into(), key.clone());
    }
    if save && !exact {
        if restore {
            job.post.push(PostAction::CacheSave {
                key,
                version,
                paths,
            });
        } else {
            run_post(
                job,
                PostAction::CacheSave {
                    key,
                    version,
                    paths,
                },
            )
            .await?;
        }
    }
    Ok(StepOutcome::success(outputs))
}

fn node_str(n: Option<&Node>) -> Option<String> {
    n.and_then(|n| n.as_str().map(str::to_string))
}

fn tmpl(s: &str) -> Result<Template> {
    Template::parse(s).map_err(|e| ForgeError::invalid("bad_expression", e.message))
}

/// An action's metadata file (`action.yml` or `action.yaml`) in `dir`.
fn read_action(label: &str, workspace: &Path, dir: &Path) -> Result<Node> {
    let file = ["action.yml", "action.yaml"]
        .iter()
        .map(|f| safe_join(workspace, dir, f))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            ForgeError::not_found(format!(
                "{label}/action.yml (check out the repository first for local actions)"
            ))
        })?;
    let src = std::fs::read_to_string(&file)
        .map_err(|e| ForgeError::internal(format!("{}: {e}", file.display())))?;
    bee_workflow::yaml::parse(&src)
        .map_err(|e| ForgeError::invalid("invalid_action", e.to_string()))
}

fn action_root(job: &Job, info: &ActionInfo) -> PathBuf {
    if info.repository.is_empty() {
        job.workspace.clone()
    } else {
        job.root.clone()
    }
}

/// Finds container work reachable through local composite actions before any non-checkout step
/// executes. Remote actions deliberately remain out of scope: the local runner does not fetch
/// them during preflight, and rejects them when their step is reached.
pub(crate) fn local_container_request(job: &Job, steps: &[StepSpec]) -> Result<Option<String>> {
    let mut visited = BTreeSet::new();
    let mut active = BTreeSet::new();
    for step in steps {
        let StepKind::Uses { action, .. } = &step.kind else {
            continue;
        };
        if let Some(request) =
            local_action_container_request(job, action, 0, &mut visited, &mut active)?
        {
            return Ok(Some(request));
        }
    }
    Ok(None)
}

fn local_action_container_request(
    job: &Job,
    action: &str,
    depth: usize,
    visited: &mut BTreeSet<String>,
    active: &mut BTreeSet<String>,
) -> Result<Option<String>> {
    if action.starts_with("docker://") {
        return Ok(Some(action.to_string()));
    }
    if !action.starts_with("./") {
        return Ok(None);
    }
    if depth >= 10 {
        return Err(ForgeError::invalid(
            "local_action_depth",
            format!("local action nesting exceeds the limit of 10 at {action}"),
        ));
    }
    if active.contains(action) {
        return Err(ForgeError::invalid(
            "local_action_cycle",
            format!("local action cycle detected at {action}"),
        ));
    }
    if visited.contains(action) {
        return Ok(None);
    }

    active.insert(action.to_string());
    let result = (|| {
        let dir = safe_join(&job.workspace, &job.workspace, action)?;
        let doc = read_action(action, &job.workspace, &dir)?;
        let using =
            node_str(doc.get("runs").and_then(|runs| runs.get("using"))).unwrap_or_default();
        if using == "docker" {
            return Ok(Some(format!("Docker action {action}")));
        }
        if using != "composite" {
            return Ok(None);
        }
        for step in doc
            .get("runs")
            .and_then(|runs| runs.get("steps"))
            .and_then(|steps| steps.as_seq())
            .unwrap_or_default()
        {
            let Some(uses) = node_str(step.get("uses")) else {
                continue;
            };
            if let Some(request) =
                local_action_container_request(job, &uses, depth + 1, visited, active)?
            {
                return Ok(Some(format!("{request} in {action}")));
            }
        }
        Ok(None)
    })();
    active.remove(action);
    if result.as_ref().is_ok_and(Option::is_none) {
        visited.insert(action.to_string());
    }
    result
}

/// Built-in actions served without fetching anything.
fn builtin(action: &str) -> bool {
    let (name, _) = base_name(action);
    matches!(
        name.as_str(),
        "actions/checkout"
            | "actions/upload-artifact"
            | "actions/download-artifact"
            | "actions/cache"
            | "actions/cache/restore"
            | "actions/cache/save"
    ) || name.starts_with("actions/setup-")
}

/// A local action (`uses: ./path`) from the workspace.
async fn local_action(
    job: &mut Job,
    action: &str,
    with: BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<StepOutcome> {
    let dir = safe_join(&job.workspace, &job.workspace, action)?;
    let info = ActionInfo {
        path: dir,
        repository: String::new(),
        git_ref: String::new(),
    };
    run_action_dir(job, action, info, with, env, timeout).await
}

/// Fetches a remote action's repository once per job; returns its directory and info.
async fn fetch_action(job: &mut Job, action: &str, r: &ActionRef) -> Result<ActionInfo> {
    let repo_dir = match job.fetched.get(action) {
        Some(d) => d.clone(),
        None => {
            let Some(reg) = job.config.actions.clone() else {
                return Err(unsupported(
                    action,
                    "actions from other repositories without an action registry (RunnerConfig::actions)",
                ));
            };
            let a = job.a.clone();
            let fetched = reg.0.fetch(&a, r).await?;
            let safe = |s: &str| {
                s.chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>()
            };
            let dir = job
                .root
                .join("_actions")
                .join(safe(&r.owner))
                .join(safe(&r.repo))
                .join(safe(&r.git_ref));
            for f in &fetched.files {
                let p = safe_join(&job.root, &dir, &f.path)?;
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| ForgeError::internal(format!("action fetch: {e}")))?;
                }
                std::fs::write(&p, &f.bytes)
                    .map_err(|e| ForgeError::internal(format!("action fetch: {e}")))?;
                #[cfg(unix)]
                if f.executable {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
                }
            }
            job.line(&format!(
                "Fetched {}@{} ({}, {} files)",
                r.repository(),
                r.git_ref,
                &fetched.changeset[..fetched.changeset.len().min(12)],
                fetched.files.len()
            ))
            .await?;
            job.fetched.insert(action.to_string(), dir.clone());
            dir
        }
    };
    Ok(ActionInfo {
        path: safe_join(&job.root, &repo_dir, &r.path)?,
        repository: r.repository(),
        git_ref: r.git_ref.clone(),
    })
}

/// An action of another repository (`uses: owner/repo[/path]@ref`).
async fn remote_action(
    job: &mut Job,
    action: &str,
    with: BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<StepOutcome> {
    let Some(r) = ActionRef::parse(action) else {
        return Err(ForgeError::invalid(
            "invalid_action",
            format!("{action} is not owner/repo[/path]@ref"),
        ));
    };
    let info = fetch_action(job, action, &r).await?;
    run_action_dir(job, action, info, with, env, timeout).await
}

/// Runs the action in `info.path` by its `runs.using`.
async fn run_action_dir(
    job: &mut Job,
    action: &str,
    info: ActionInfo,
    with: BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<StepOutcome> {
    let root = action_root(job, &info);
    let doc = read_action(action, &root, &info.path)?;
    let using = node_str(doc.get("runs").and_then(|r| r.get("using"))).unwrap_or_default();
    if using == "composite" {
        composite(job, action, &doc, info, with, env).await
    } else if using.starts_with("node") {
        javascript(job, action, &doc, info, with, env, timeout).await
    } else if using == "docker" {
        Err(unsupported(action, "Docker actions"))
    } else {
        Err(unsupported(
            action,
            &format!("actions with `runs.using: {using}`"),
        ))
    }
}

/// The `inputs` of an action call: given values, then defaults (which may be expressions);
/// a missing required input is `bad_input`.
fn action_inputs(
    job: &Job,
    action: &str,
    doc: &Node,
    with: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> Result<Object> {
    let mut inputs = Object::new();
    let outer = Scope::default();
    if let Some(decl) = doc.get("inputs").and_then(|i| i.as_map()) {
        for (k, v) in decl {
            let Some(name) = k.as_str() else { continue };
            let val = match with.get(name) {
                Some(x) => x.clone(),
                None => match node_str(v.get("default")) {
                    Some(d) => {
                        let ctx = job.ctx(&outer, env);
                        tmpl(&d)?.evaluate_string(&ctx).unwrap_or_default()
                    }
                    None => {
                        if v.get("required").and_then(|r| r.as_str()) == Some("true") {
                            return Err(ForgeError::invalid(
                                "bad_input",
                                format!("{action}: input {name} is required"),
                            ));
                        }
                        String::new()
                    }
                },
            };
            inputs.insert(name.to_string(), Value::from(val));
        }
    }
    for (k, v) in with {
        if !inputs.contains_key(k) {
            inputs.insert(k.clone(), Value::from(v.as_str()));
        }
    }
    Ok(inputs)
}

/// `INPUT_<NAME>` variables (upper case, spaces as `_`) for a JavaScript action.
fn input_env(inputs: &Object) -> BTreeMap<String, String> {
    inputs
        .iter()
        .map(|(k, v)| {
            (
                format!("INPUT_{}", k.replace(' ', "_").to_uppercase()),
                v.to_display_string(),
            )
        })
        .collect()
}

fn state_env(state: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    state
        .iter()
        .map(|(k, v)| (format!("STATE_{k}"), v.clone()))
        .collect()
}

/// A script of a JavaScript action (`main`, `pre` or `post`) inside the action directory.
fn script_in(
    action: &str,
    workspace: &Path,
    dir: &Path,
    doc: &Node,
    key: &str,
) -> Result<Option<PathBuf>> {
    match node_str(doc.get("runs").and_then(|r| r.get(key))) {
        Some(rel) if !rel.trim().is_empty() => {
            let p = safe_join(workspace, dir, rel.trim())?;
            if !p.is_file() {
                return Err(ForgeError::not_found(format!(
                    "{action}: runs.{key} file {rel}"
                )));
            }
            Ok(Some(p))
        }
        _ => Ok(None),
    }
}

/// Whether the action's `pre-if`/`post-if` (default `always()`) holds now.
fn condition_holds(job: &Job, doc: &Node, key: &str, env: &BTreeMap<String, String>) -> bool {
    let cond =
        node_str(doc.get("runs").and_then(|r| r.get(key))).unwrap_or_else(|| "always()".into());
    let ctx = job.ctx(&Scope::default(), env);
    Condition::parse(&cond)
        .ok()
        .and_then(|c| c.evaluate(&ctx).ok())
        .unwrap_or(false)
}

/// Runs one script of a JavaScript action with `node`.
async fn run_node(
    job: &mut Job,
    info: &ActionInfo,
    script: &Path,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<StepOutcome> {
    let Some(node) = find_on_path(job, &["node"]) else {
        job.line("##[error]JavaScript actions need `node` on PATH; the local runner does not download Node.js")
            .await?;
        return Ok(StepOutcome::failure());
    };
    let argv = vec![
        node.to_string_lossy().into_owned(),
        script.to_string_lossy().into_owned(),
    ];
    let prev = job.action.replace(info.clone());
    let workspace = job.workspace.clone();
    let res = job.run_argv(&argv, &workspace, env, timeout).await;
    job.action = prev;
    res
}

/// A JavaScript action: `main` runs now with the inputs as `INPUT_*`; state saved by `pre`
/// (at job start) reaches it as `STATE_*`; `post` is scheduled for the end of the job.
async fn javascript(
    job: &mut Job,
    action: &str,
    doc: &Node,
    info: ActionInfo,
    with: BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<StepOutcome> {
    let root = action_root(job, &info);
    let main = script_in(action, &root, &info.path, doc, "main")?.ok_or_else(|| {
        ForgeError::invalid("invalid_action", format!("{action}: runs.main is missing"))
    })?;
    let post = script_in(action, &root, &info.path, doc, "post")?;
    let inputs = action_inputs(job, action, doc, &with, env)?;
    let mut call_env = env.clone();
    call_env.extend(input_env(&inputs));
    let key = format!("{}|{action}", job.step_key);
    let mut state = match job.pre_states.remove(&key) {
        Some(s) => s,
        None => match script_in(action, &root, &info.path, doc, "pre")? {
            Some(pre) if condition_holds(job, doc, "pre-if", env) => {
                let timeout = Some(job.remaining_timeout());
                let outcome = run_node(job, &info, &pre, &call_env, timeout).await?;
                if !outcome.ok {
                    job.line(&format!("##[error]Pre {action} failed")).await?;
                    return Ok(StepOutcome::failure());
                }
                std::mem::take(&mut job.last_state)
            }
            _ => BTreeMap::new(),
        },
    };
    let mut main_env = call_env.clone();
    main_env.extend(state_env(&state));
    let outcome = run_node(job, &info, &main, &main_env, timeout).await?;
    state.extend(std::mem::take(&mut job.last_state));
    if let Some(post) = post {
        job.post.push(PostAction::Node {
            label: action.to_string(),
            info,
            script: post,
            env: call_env,
            state,
            condition: node_str(doc.get("runs").and_then(|r| r.get("post-if")))
                .unwrap_or_else(|| "always()".into()),
        });
    }
    Ok(outcome)
}

/// Runs the `pre` scripts of the job's remote JavaScript actions before the first step
/// (`pre-if`, default `always()`); their saved state reaches `main` and `post`. A failing
/// `pre` fails the job. Actions that cannot be fetched now report their error when their
/// step runs.
pub async fn run_pres(job: &mut Job, steps: &[StepSpec], scope: &Scope) -> Result<()> {
    for (i, s) in steps.iter().enumerate() {
        let StepKind::Uses { action, with } = &s.kind else {
            continue;
        };
        if builtin(action) || action.starts_with("./") || action.starts_with("docker://") {
            continue;
        }
        let Some(r) = ActionRef::parse(action) else {
            continue;
        };
        if job.config.actions.is_none() {
            continue;
        }
        let Ok(info) = fetch_action(job, action, &r).await else {
            continue;
        };
        let root = action_root(job, &info);
        let Ok(doc) = read_action(action, &root, &info.path) else {
            continue;
        };
        let using = node_str(doc.get("runs").and_then(|r| r.get("using"))).unwrap_or_default();
        if !using.starts_with("node") {
            continue;
        }
        let Some(pre) = script_in(action, &root, &info.path, &doc, "pre")? else {
            continue;
        };
        let mut env = BTreeMap::new();
        for (k, v) in &s.env {
            let ctx = job.ctx(scope, &env);
            env.insert(k.clone(), v.evaluate_string(&ctx).unwrap_or_default());
        }
        let key = format!("step{}|{action}", i + 1);
        if !condition_holds(job, &doc, "pre-if", &env) {
            job.pre_states.insert(key, BTreeMap::new());
            continue;
        }
        let ctx = job.ctx(scope, &env);
        let mut w = BTreeMap::new();
        for (k, v) in with {
            w.insert(k.clone(), v.evaluate_string(&ctx).unwrap_or_default());
        }
        let inputs = action_inputs(job, action, &doc, &w, &env)?;
        env.extend(input_env(&inputs));
        job.line(&format!("##[group]Pre {action}")).await?;
        job.line("##[endgroup]").await?;
        let timeout = Some(job.remaining_timeout());
        let outcome = run_node(job, &info, &pre, &env, timeout).await?;
        let state = std::mem::take(&mut job.last_state);
        job.pre_states.insert(key, state);
        if !outcome.ok {
            job.line(&format!("##[error]Pre {action} failed")).await?;
            job.failed = true;
            return Ok(());
        }
    }
    Ok(())
}

async fn composite(
    job: &mut Job,
    action: &str,
    doc: &Node,
    info: ActionInfo,
    with: BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> Result<StepOutcome> {
    let inputs = action_inputs(job, action, doc, &with, env)?;
    let mut steps = Vec::new();
    for s in doc
        .get("runs")
        .and_then(|r| r.get("steps"))
        .and_then(|s| s.as_seq())
        .unwrap_or_default()
    {
        let env_pairs: Vec<(String, Template)> = s
            .get("env")
            .and_then(|e| e.as_map())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        Some((
                            k.as_str()?.to_string(),
                            tmpl(v.as_str().unwrap_or("")).ok()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let kind = if let Some(run) = node_str(s.get("run")) {
            let shell = node_str(s.get("shell")).ok_or_else(|| {
                ForgeError::invalid(
                    "invalid_action",
                    format!("{action}: composite `run` steps need `shell`"),
                )
            })?;
            let command = bee_workflow::plan::shell_command(&shell)
                .map(str::to_string)
                .unwrap_or_else(|| shell.clone());
            StepKind::Run {
                script: tmpl(&run)?,
                shell,
                command,
                working_directory: node_str(s.get("working-directory"))
                    .map(|w| tmpl(&w))
                    .transpose()?,
            }
        } else if let Some(uses) = node_str(s.get("uses")) {
            let with: Vec<(String, Template)> = s
                .get("with")
                .and_then(|w| w.as_map())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| {
                            Some((
                                k.as_str()?.to_string(),
                                tmpl(v.as_str().unwrap_or("")).ok()?,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            StepKind::Uses { action: uses, with }
        } else {
            return Err(ForgeError::invalid(
                "invalid_action",
                format!("{action}: a step needs `run` or `uses`"),
            ));
        };
        let name = node_str(s.get("name")).unwrap_or_else(|| match &kind {
            StepKind::Run { script, .. } => {
                format!("Run {}", script.render().lines().next().unwrap_or(""))
            }
            StepKind::Uses { action, .. } => format!("Run {action}"),
        });
        steps.push(StepSpec {
            id: node_str(s.get("id")),
            name: tmpl(&name)?,
            condition: node_str(s.get("if")).unwrap_or_default(),
            kind,
            env: env_pairs,
            continue_on_error: tmpl(&node_str(s.get("continue-on-error")).unwrap_or_default())?,
            timeout_minutes: None,
        });
    }
    let mut scope = Scope {
        steps: Object::new(),
        inputs: Some(Value::from(inputs)),
    };
    let prev_action = job.action.replace(info);
    let failed_before = job.failed;
    job.failed = false;
    let res = Box::pin(job.run_steps(&steps, &mut scope, false)).await;
    let ok = !job.failed;
    job.failed = failed_before;
    let outputs = res.and_then(|()| {
        let mut outputs = BTreeMap::new();
        if let Some(decl) = doc.get("outputs").and_then(|o| o.as_map()) {
            let ctx = job.ctx(&scope, env);
            for (k, v) in decl {
                if let (Some(name), Some(val)) = (k.as_str(), node_str(v.get("value"))) {
                    outputs.insert(
                        name.to_string(),
                        tmpl(&val)?.evaluate_string(&ctx).unwrap_or_default(),
                    );
                }
            }
        }
        Ok(outputs)
    });
    job.action = prev_action;
    Ok(StepOutcome {
        ok,
        outputs: outputs?,
    })
}

#[cfg(test)]
mod tests {
    use super::safe_join;

    #[test]
    fn safe_join_rejects_parent_components_with_both_separators() {
        let workspace = tempfile::tempdir().unwrap();
        for path in [r"..\x", r"a\..\..\x", "a/../../x"] {
            assert!(
                safe_join(workspace.path(), workspace.path(), path).is_err(),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_join_allows_symlinks_that_resolve_inside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("target")).unwrap();
        std::os::unix::fs::symlink("target", workspace.path().join("alias")).unwrap();
        let resolved = safe_join(workspace.path(), workspace.path(), "alias").unwrap();
        assert!(resolved.starts_with(workspace.path()));
        assert!(resolved.is_dir());
    }
}
