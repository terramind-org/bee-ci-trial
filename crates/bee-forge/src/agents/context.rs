//! Agent-facing context: repo instructions, memory and context bundles.
//!
//! # Instructions
//!
//! [`Agents::resolve_instructions`] returns, in precedence order (most specific last):
//! org instructions (the org-scope memory under key `instructions`),
//! `.github/copilot-instructions.md`, `.github/instructions/*.instructions.md` whose
//! `applyTo` front matter matches one of the paths, then for every directory from the root to
//! each path's directory its `AGENTS.md` (or `CLAUDE.md` when the runtime is `claude-code` and
//! the directory has one), then the named custom agent from `.github/agents/`. Lines of the
//! form `@path` pull in another file once (recorded in `includes`).
//!
//! # Memory
//!
//! Key/value documents per repo, org or agent principal, versioned (row version), with tags,
//! sources and an optional time to live. Expired memories are invisible at once and removed
//! by [`Agents::maintain`]. Search ranks by term matches in key, tags and value.
//!
//! # Context bundles
//!
//! Named lists of paths, subjects (issues, CRs), URLs and notes, private to their owner unless
//! shared. Materializing resolves path items to blob ids at a revision and subject items to
//! titles and bodies, with a per-item status.

use std::collections::BTreeSet;

use bee_proto::v1::{
    AgentInstructionFile, AgentInstructionSource as Src, AgentMemory, AgentMemoryScope,
    ContextBundle, ContextBundleItem, ContextBundleResolvedFile, ContextBundleResolvedItem,
    ItemStatus, MaterializeContextBundleResponse, ObjectId, Repo, SubjectKind, agent_memory_scope,
    context_bundle_item,
};

use super::registry::{locate, put_locator};
use super::*;
use crate::audit::AuditDraft;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan, Tx};
use crate::work::RepoInfo;

/// Largest memory value.
pub const MAX_MEMORY_VALUE: usize = 256 * 1024;
/// Most memories per scope.
pub const MAX_MEMORIES: u64 = 10_000;
/// Most items in a bundle.
pub const MAX_BUNDLE_ITEMS: usize = 200;
/// Most bundles per scope.
pub const MAX_BUNDLES: u64 = 1_000;
/// Most instruction files read for one resolution.
pub const MAX_INSTRUCTION_FILES: usize = 64;

/// Resolved instructions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedInstructions {
    /// Files in precedence order, most specific last.
    pub files: Vec<AgentInstructionFile>,
    /// Concatenated Markdown with file headers.
    pub combined: String,
    /// The changeset read.
    pub changeset: ObjectId,
}

/// Front matter of an instruction file: `name` and `applyTo` globs.
pub fn front_matter(text: &str) -> (String, Vec<String>, String) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (String::new(), Vec::new(), text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (String::new(), Vec::new(), text.to_string());
    };
    let head = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']).to_string();
    let mut name = String::new();
    let mut apply: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in head.lines() {
        let t = line.trim();
        if in_list && let Some(item) = t.strip_prefix("- ") {
            apply.push(unquote(item));
            continue;
        }
        in_list = false;
        if let Some((k, v)) = t.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            match k {
                "name" => name = unquote(v),
                "applyTo" | "apply_to" | "globs" => {
                    if v.is_empty() {
                        in_list = true;
                    } else {
                        let v = v.trim_start_matches('[').trim_end_matches(']');
                        apply.extend(v.split(',').map(unquote).filter(|s| !s.is_empty()));
                    }
                }
                _ => {}
            }
        }
    }
    (name, apply, body)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

fn glob_match(globs: &[String], paths: &[String]) -> bool {
    globs.iter().any(|g| {
        let g = if g == "**" || g == "**/*" {
            "**".to_string()
        } else {
            g.clone()
        };
        match bee_policy::Glob::parse(&g) {
            Ok(glob) => paths.iter().any(|p| p.is_empty() || glob.matches(p)) || g == "**",
            Err(_) => false,
        }
    })
}

/// The kind of instruction file a path is, if any.
pub fn instruction_source(path: &str) -> Option<Src> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if path == ".github/copilot-instructions.md" {
        Some(Src::CopilotInstructions)
    } else if path.starts_with(".github/agents/")
        && name.ends_with(".md")
        && !path[15..].contains('/')
    {
        Some(Src::CustomAgent)
    } else if path.starts_with(".github/instructions/") && name.ends_with(".instructions.md") {
        Some(Src::PathInstructions)
    } else if name == "AGENTS.md" {
        Some(Src::AgentsMd)
    } else if name == "CLAUDE.md" {
        Some(Src::ClaudeMd)
    } else {
        None
    }
}

fn memory_scope_key(s: &AgentMemoryScope) -> Result<String> {
    match &s.scope {
        Some(agent_memory_scope::Scope::Repo(r)) => match &r.repo {
            Some(bee_proto::v1::repo_ref::Repo::Id(id)) => Ok(format!("repo:{id}")),
            _ => Err(ForgeError::invalid(
                "invalid_scope",
                "memory scopes name repos by id (resolve first)",
            )),
        },
        Some(agent_memory_scope::Scope::Org(o)) if !o.is_empty() => Ok(format!("org:{o}")),
        Some(agent_memory_scope::Scope::AgentPrincipalId(a)) if !a.is_empty() => {
            Ok(format!("agent:{a}"))
        }
        _ => Err(ForgeError::missing("scope")),
    }
}

fn check_key(k: &str) -> Result<()> {
    if k.is_empty() {
        return Err(ForgeError::missing("key"));
    }
    if k.len() > 256 || k.chars().any(char::is_control) {
        return Err(ForgeError::invalid(
            "invalid_key",
            "keys are 1 to 256 printable characters",
        ));
    }
    Ok(())
}

fn live(m: &AgentMemory, now: u64) -> bool {
    m.expires_at
        .as_ref()
        .is_none_or(|t| crate::ids::ts_ms(t) > now)
}

fn terms(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Relevance of a memory for a query: key matches weigh 3, tags 2, value 1, normalized by
/// the number of query terms.
pub fn memory_score(m: &AgentMemory, query: &str) -> f64 {
    let q: BTreeSet<String> = terms(query).into_iter().collect();
    if q.is_empty() {
        return 0.0;
    }
    let key: BTreeSet<String> = terms(&m.key).into_iter().collect();
    let tags: BTreeSet<String> = m.tags.iter().flat_map(|t| terms(t)).collect();
    let value = terms(&m.value);
    let mut score = 0.0;
    for t in &q {
        if key.contains(t) {
            score += 3.0;
        }
        if tags.contains(t) {
            score += 2.0;
        }
        let n = value.iter().filter(|v| *v == t).count();
        if n > 0 {
            score += 1.0 + (n as f64).ln();
        }
    }
    score / q.len() as f64
}

fn check_item(it: &ContextBundleItem) -> Result<ContextBundleItem> {
    let mut it = it.clone();
    match &it.item {
        Some(context_bundle_item::Item::Path(p)) => {
            let n = crate::leases::normalize_path(p)?;
            it.item = Some(context_bundle_item::Item::Path(n.into_bytes()));
        }
        Some(context_bundle_item::Item::Subject(s)) => {
            if s.id.is_empty() {
                return Err(ForgeError::missing("items.subject.id"));
            }
        }
        Some(context_bundle_item::Item::Url(u)) => {
            if !(u.starts_with("https://") || u.starts_with("http://")) || u.len() > 2048 {
                return Err(ForgeError::invalid(
                    "bad_url",
                    format!("{u:?} is not an http(s) URL"),
                ));
            }
        }
        Some(context_bundle_item::Item::Note(n)) => check_len("items.note", n, 16 * 1024)?,
        None => return Err(ForgeError::missing("items.item")),
    }
    check_len("items.label", &it.label, 256)?;
    Ok(it)
}

fn bundle_scope(b: &ContextBundle) -> String {
    match b.repo.as_ref().and_then(|r| r.repo.as_ref()) {
        Some(bee_proto::v1::repo_ref::Repo::Id(id)) => format!("repo:{id}"),
        _ => format!("org:{}", b.org),
    }
}

impl Agents {
    // ---- instructions ----

    /// Resolves the instructions that apply to `paths` at changeset `at`.
    pub async fn resolve_instructions(
        &self,
        cx: &Cx,
        repo: &Repo,
        at: &ObjectId,
        paths: &[String],
        runtime: &str,
        custom_agent: &str,
    ) -> Result<ResolvedInstructions> {
        let r = &self.ports.repo;
        let mut files: Vec<AgentInstructionFile> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        // Org instructions.
        if let Ok(m) = self
            .get_memory(
                cx,
                &AgentMemoryScope {
                    scope: Some(agent_memory_scope::Scope::Org(repo.org_id.clone())),
                },
                "instructions",
            )
            .await
        {
            files.push(AgentInstructionFile {
                source: Src::Org as i32,
                name: "org".into(),
                content: m.value,
                ..Default::default()
            });
        }
        let paths: Vec<String> = paths
            .iter()
            .map(|p| p.trim_matches('/').to_string())
            .collect();
        let add = |f: Option<AgentInstructionFile>,
                   files: &mut Vec<AgentInstructionFile>,
                   seen: &mut BTreeSet<String>| {
            if let Some(f) = f {
                let p = String::from_utf8_lossy(&f.path).into_owned();
                if files.len() < MAX_INSTRUCTION_FILES && seen.insert(p) {
                    files.push(f);
                }
            }
        };
        let f = self
            .instruction_file(repo, at, ".github/copilot-instructions.md", true)
            .await?;
        add(f, &mut files, &mut seen);
        let (list, _) = r
            .list_files(repo, at, ".github/instructions", 500)
            .await
            .unwrap_or_default();
        for e in list {
            if !e.path.ends_with(".instructions.md") {
                continue;
            }
            if let Some(f) = self.instruction_file(repo, at, &e.path, true).await?
                && (f.applies_to.is_empty() || glob_match(&f.applies_to, &paths))
            {
                add(Some(f), &mut files, &mut seen);
            }
        }
        // Directories from the root to every path's directory, shallow first.
        let mut dirs: BTreeSet<(usize, String)> = BTreeSet::new();
        dirs.insert((0, String::new()));
        for p in &paths {
            let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
            // Paths name files or directories; a directory's own AGENTS.md applies too.
            for i in 1..=comps.len() {
                dirs.insert((i, comps[..i].join("/")));
            }
        }
        for (_, d) in dirs {
            let join = |n: &str| {
                if d.is_empty() {
                    n.to_string()
                } else {
                    format!("{d}/{n}")
                }
            };
            let claude = if runtime == "claude-code" {
                self.instruction_file(repo, at, &join("CLAUDE.md"), true)
                    .await?
            } else {
                None
            };
            if claude.is_some() {
                add(claude, &mut files, &mut seen);
            } else {
                let f = self
                    .instruction_file(repo, at, &join("AGENTS.md"), true)
                    .await?;
                add(f, &mut files, &mut seen);
            }
        }
        if !custom_agent.is_empty() {
            let (list, _) = r
                .list_files(repo, at, ".github/agents", 500)
                .await
                .unwrap_or_default();
            let mut found = false;
            for e in list {
                if instruction_source(&e.path) != Some(Src::CustomAgent) {
                    continue;
                }
                if let Some(f) = self.instruction_file(repo, at, &e.path, true).await? {
                    let stem = e
                        .path
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches(".md")
                        .trim_end_matches(".agent");
                    if f.name == custom_agent || stem == custom_agent {
                        add(Some(f), &mut files, &mut seen);
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                return Err(ForgeError::not_found(format!(
                    "custom agent {custom_agent}"
                )));
            }
        }
        let mut combined = String::new();
        for f in &files {
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            let label = if f.path.is_empty() {
                f.name.clone()
            } else {
                String::from_utf8_lossy(&f.path).into_owned()
            };
            combined.push_str(&format!("<!-- {label} -->\n{}", f.content.trim_end()));
        }
        Ok(ResolvedInstructions {
            files,
            combined,
            changeset: at.clone(),
        })
    }

    /// Reads one instruction file (front matter parsed, `@path` includes expanded once).
    async fn instruction_file(
        &self,
        repo: &Repo,
        at: &ObjectId,
        path: &str,
        content: bool,
    ) -> Result<Option<AgentInstructionFile>> {
        let Some(src) = instruction_source(path) else {
            return Ok(None);
        };
        // A path through a file (`a.rs/AGENTS.md`) is simply absent.
        let Ok(Some((blob, bytes))) = self.ports.repo.read_file(repo, at, path).await else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let (name, applies_to, body) = front_matter(&text);
        let mut includes = Vec::new();
        let mut out = String::new();
        for line in body.lines() {
            let t = line.trim();
            if let Some(inc) = t.strip_prefix('@')
                && !inc.is_empty()
                && !inc.contains(' ')
                && !inc.contains("..")
            {
                let inc = inc.trim_start_matches('/');
                let base = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let candidates = [
                    inc.to_string(),
                    if base.is_empty() {
                        inc.to_string()
                    } else {
                        format!("{base}/{inc}")
                    },
                ];
                let mut done = false;
                for c in candidates {
                    if let Ok(Some((_, b))) = self.ports.repo.read_file(repo, at, &c).await {
                        if content {
                            out.push_str(&format!(
                                "<!-- @{c} -->\n{}\n",
                                String::from_utf8_lossy(&b)
                            ));
                        }
                        includes.push(c.into_bytes());
                        done = true;
                        break;
                    }
                }
                if done {
                    continue;
                }
            }
            if content {
                out.push_str(line);
                out.push('\n');
            }
        }
        Ok(Some(AgentInstructionFile {
            path: path.as_bytes().to_vec(),
            source: src as i32,
            blob: Some(blob),
            name,
            applies_to,
            content: if content { out } else { String::new() },
            includes,
        }))
    }

    /// Every instruction and custom agent file in the repo at `at`.
    pub async fn list_instruction_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
        include_content: bool,
    ) -> Result<Vec<AgentInstructionFile>> {
        let (all, _) = self.ports.repo.list_files(repo, at, "", 200_000).await?;
        let mut out = Vec::new();
        for f in all {
            if instruction_source(&f.path).is_some()
                && let Some(mut file) = self.instruction_file(repo, at, &f.path, true).await?
            {
                if !include_content {
                    file.content.clear();
                }
                out.push(file);
            }
        }
        Ok(out)
    }

    // ---- memory ----

    /// Creates or replaces a memory. `expected_version` 0 = any.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_memory(
        &self,
        cx: &Cx,
        org_id: &str,
        mut m: AgentMemory,
        expected_version: u64,
        ttl_seconds: u64,
    ) -> Result<AgentMemory> {
        let scope = memory_scope_key(&m.scope.clone().unwrap_or_default())?;
        check_key(&m.key)?;
        if m.value.len() > MAX_MEMORY_VALUE {
            return Err(ForgeError::too_large(
                "memory_value",
                MAX_MEMORY_VALUE as u64,
                m.value.len() as u64,
            ));
        }
        if m.content_type.is_empty() {
            m.content_type = "text/plain".into();
        }
        if !["text/plain", "text/markdown", "application/json"].contains(&m.content_type.as_str()) {
            return Err(ForgeError::invalid(
                "invalid_content_type",
                "content_type is text/plain, text/markdown or application/json",
            ));
        }
        if m.content_type == "application/json"
            && serde_json::from_str::<serde_json::Value>(&m.value).is_err()
        {
            return Err(ForgeError::invalid("invalid_json", "value is not JSON"));
        }
        if m.tags.len() > 20 || m.sources.len() > 50 {
            return Err(ForgeError::too_large(
                "memory_tags",
                20,
                m.tags.len() as u64,
            ));
        }
        m.tags.sort();
        m.tags.dedup();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let cur = tx.get_msg::<AgentMemory>(&MEMORIES, &scope, &m.key).await?;
        let live_cur = cur.as_ref().filter(|(c, _)| live(c, now));
        let current_version = live_cur.map(|(c, _)| c.version).unwrap_or(0);
        if expected_version != 0 && expected_version != current_version {
            return Err(ForgeError::version_mismatch(
                expected_version,
                current_version,
            ));
        }
        if cur.is_none() && tx.count(&MEMORIES, &scope, None).await? >= MAX_MEMORIES {
            return Err(ForgeError::too_large(
                "memories",
                MAX_MEMORIES,
                MAX_MEMORIES + 1,
            ));
        }
        m.author = Some(cx.principal());
        m.version = current_version + 1;
        m.created_at = live_cur.and_then(|(c, _)| c.created_at).or(Some(ts(now)));
        m.updated_at = Some(ts(now));
        m.expires_at = (ttl_seconds > 0).then(|| ts(now + ttl_seconds * 1000));
        let mut row = Row::new(scope.clone(), m.key.clone()).msg(&m);
        if ttl_seconds > 0 {
            row = row.n1((now + ttl_seconds * 1000) as i64);
        }
        match &cur {
            Some((_, r)) => {
                tx.update(&MEMORIES, &row.version(r.version)).await?;
            }
            None => tx.insert(&MEMORIES, &row).await?,
        }
        self.record_memory(&mut tx, org_id, &scope, &m.key, "agent.memory_put", true)
            .await?;
        tx.commit().await?;
        Ok(m)
    }

    async fn record_memory(
        &self,
        tx: &mut Tx,
        org_id: &str,
        scope: &str,
        key: &str,
        action: &str,
        written: bool,
    ) -> Result<()> {
        let json = serde_json::json!({"scope": scope, "key": key, "written": written}).to_string();
        let repo_id = scope.strip_prefix("repo:").unwrap_or("").to_string();
        if !org_id.is_empty() {
            tx.emit_event(bee_proto::v1::Event {
                kind: "agent_memory.updated".into(),
                repo_id: repo_id.clone(),
                org_id: org_id.to_string(),
                subject: Some(bee_proto::v1::SubjectRef {
                    kind: if repo_id.is_empty() {
                        SubjectKind::Org
                    } else {
                        SubjectKind::Repo
                    } as i32,
                    repo: if repo_id.is_empty() {
                        None
                    } else {
                        super::registry::repo_ref_id(&repo_id)
                    },
                    id: key.to_string(),
                }),
                payload_json: json.clone(),
                ..Default::default()
            })
            .await?;
        }
        tx.audit(
            AuditDraft::new(action, org_id)
                .repo_id(&repo_id)
                .target_id(&format!("{scope}/{key}"))
                .after(&json),
        )
        .await
    }

    /// One memory.
    pub async fn get_memory(
        &self,
        cx: &Cx,
        scope: &AgentMemoryScope,
        key: &str,
    ) -> Result<AgentMemory> {
        let s = memory_scope_key(scope)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        tx.get_msg::<AgentMemory>(&MEMORIES, &s, key)
            .await?
            .map(|(m, _)| m)
            .filter(|m| live(m, now))
            .ok_or_else(|| ForgeError::not_found(format!("memory {key}")))
    }

    /// Memories of a scope by key, filtered by prefix and tags (all must match).
    pub async fn list_memories(
        &self,
        cx: &Cx,
        scope: &AgentMemoryScope,
        prefix: &str,
        tags: &[String],
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AgentMemory>, String)> {
        let s = memory_scope_key(scope)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let mut out = Vec::new();
        let mut cursor = (!after.is_empty()).then(|| crate::store::Cursor {
            n1: 0,
            scope: s.clone(),
            id: after.to_string(),
        });
        loop {
            let rows = tx
                .scan_msgs::<AgentMemory>(
                    &MEMORIES,
                    &Scan::scope(s.clone(), 500).after(cursor.clone()),
                )
                .await?;
            let n = rows.len();
            for (m, row) in rows {
                cursor = Some(crate::store::Cursor::after(&row));
                if live(&m, now)
                    && m.key.starts_with(prefix)
                    && tags.iter().all(|t| m.tags.contains(t))
                {
                    out.push(m);
                    if out.len() > limit {
                        out.truncate(limit);
                        let next = out.last().map(|m| m.key.clone()).unwrap_or_default();
                        return Ok((out, next));
                    }
                }
            }
            if n < 500 {
                return Ok((out, String::new()));
            }
        }
    }

    /// Deletes a memory.
    pub async fn delete_memory(
        &self,
        cx: &Cx,
        org_id: &str,
        scope: &AgentMemoryScope,
        key: &str,
    ) -> Result<()> {
        let s = memory_scope_key(scope)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        match tx.get_msg::<AgentMemory>(&MEMORIES, &s, key).await? {
            Some((m, _)) if live(&m, now) => {}
            _ => return Err(ForgeError::not_found(format!("memory {key}"))),
        }
        tx.delete(&MEMORIES, &s, key).await?;
        self.record_memory(&mut tx, org_id, &s, key, "agent.memory_delete", false)
            .await?;
        tx.commit().await
    }

    /// Memories ranked by relevance to `query` (ties by key), at most `limit`.
    pub async fn search_memories(
        &self,
        cx: &Cx,
        scope: &AgentMemoryScope,
        query: &str,
        tags: &[String],
        limit: usize,
    ) -> Result<Vec<(AgentMemory, f64)>> {
        if query.trim().is_empty() {
            return Err(ForgeError::missing("query"));
        }
        let (all, _) = self
            .list_memories(cx, scope, "", tags, "", MAX_MEMORIES as usize)
            .await?;
        let mut hits: Vec<(AgentMemory, f64)> = all
            .into_iter()
            .map(|m| {
                let s = memory_score(&m, query);
                (m, s)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.key.cmp(&b.0.key))
        });
        hits.truncate(limit.clamp(1, 100));
        Ok(hits)
    }

    /// Removes expired memories.
    pub(crate) async fn sweep_memories(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms() as i64;
        let rows = tx
            .scan(
                &MEMORIES,
                &Scan {
                    order: crate::store::Order::N1,
                    n1_max: Some(now),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        let n = rows.len();
        for r in rows {
            tx.delete(&MEMORIES, &r.scope, &r.id).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    // ---- context bundles ----

    /// Creates a bundle owned by the caller.
    pub async fn create_bundle(
        &self,
        cx: &Cx,
        org_id: &str,
        mut b: ContextBundle,
    ) -> Result<ContextBundle> {
        b.name = check_name("name", &b.name)?;
        check_len("description", &b.description, 4096)?;
        if b.items.len() > MAX_BUNDLE_ITEMS {
            return Err(ForgeError::too_large(
                "bundle_items",
                MAX_BUNDLE_ITEMS as u64,
                b.items.len() as u64,
            ));
        }
        b.items = b.items.iter().map(check_item).collect::<Result<_>>()?;
        let scope = bundle_scope(&b);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if tx.count(&BUNDLES, &scope, None).await? >= MAX_BUNDLES {
            return Err(ForgeError::too_large(
                "bundles",
                MAX_BUNDLES,
                MAX_BUNDLES + 1,
            ));
        }
        let now = tx.now_ms();
        b.id = tx.new_id()?;
        b.owner = Some(cx.principal());
        b.created_at = Some(ts(now));
        b.updated_at = Some(ts(now));
        tx.insert(
            &BUNDLES,
            &Row::new(scope.clone(), b.id.clone())
                .k1(cx.actor_id.clone())
                .n1(now as i64)
                .msg(&b),
        )
        .await?;
        put_locator(&mut tx, "bundle", &b.id, &scope).await?;
        self.record_bundle(&mut tx, org_id, &b, "agent.bundle_create")
            .await?;
        tx.commit().await?;
        Ok(b)
    }

    async fn record_bundle(
        &self,
        tx: &mut Tx,
        org_id: &str,
        b: &ContextBundle,
        action: &str,
    ) -> Result<()> {
        let repo_id = bundle_scope(b)
            .strip_prefix("repo:")
            .unwrap_or("")
            .to_string();
        let json = pjson(b);
        tx.emit_event(bee_proto::v1::Event {
            kind: "context_bundle.updated".into(),
            repo_id: repo_id.clone(),
            org_id: org_id.to_string(),
            subject: Some(bee_proto::v1::SubjectRef {
                kind: if repo_id.is_empty() {
                    SubjectKind::Org
                } else {
                    SubjectKind::Repo
                } as i32,
                repo: if repo_id.is_empty() {
                    None
                } else {
                    super::registry::repo_ref_id(&repo_id)
                },
                id: b.id.clone(),
            }),
            payload_json: json.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new(action, org_id)
                .repo_id(&repo_id)
                .target_id(&b.id)
                .after(&json),
        )
        .await
    }

    /// A bundle by id.
    pub async fn get_bundle(&self, cx: &Cx, id: &str) -> Result<ContextBundle> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::load_bundle(&mut tx, id).await?.0)
    }

    async fn load_bundle(tx: &mut Tx, id: &str) -> Result<(ContextBundle, Row)> {
        let scope = locate(tx, "bundle", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("context bundle {id}")))?;
        tx.get_msg::<ContextBundle>(&BUNDLES, &scope, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("context bundle {id}")))
    }

    /// Bundles in a repo (`repo_id`) or org, visible to `viewer` (shared or owned), filtered
    /// by owner, paged by id.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_bundles(
        &self,
        cx: &Cx,
        repo_id: &str,
        org_id: &str,
        owner_id: &str,
        viewer: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ContextBundle>, String)> {
        let scope = if repo_id.is_empty() {
            format!("org:{org_id}")
        } else {
            format!("repo:{repo_id}")
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut v: Vec<ContextBundle> = tx
            .scan_msgs::<ContextBundle>(&BUNDLES, &Scan::scope(scope, crate::store::MAX_SCAN))
            .await?
            .into_iter()
            .map(|(b, _)| b)
            .filter(|b| {
                let owner = b.owner.as_ref().map(|o| o.id.as_str()).unwrap_or("");
                (owner_id.is_empty() || owner == owner_id) && (b.shared || owner == viewer)
            })
            .filter(|b| after.is_empty() || b.id.as_str() > after)
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|b| b.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// Updates a bundle; a non-empty `items` replaces the items.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_bundle(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        name: Option<String>,
        description: Option<String>,
        items: Vec<ContextBundleItem>,
        shared: Option<bool>,
    ) -> Result<ContextBundle> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut b, row) = Self::load_bundle(&mut tx, id).await?;
        if let Some(n) = name {
            b.name = check_name("name", &n)?;
        }
        if let Some(d) = description {
            check_len("description", &d, 4096)?;
            b.description = d;
        }
        if !items.is_empty() {
            if items.len() > MAX_BUNDLE_ITEMS {
                return Err(ForgeError::too_large(
                    "bundle_items",
                    MAX_BUNDLE_ITEMS as u64,
                    items.len() as u64,
                ));
            }
            b.items = items.iter().map(check_item).collect::<Result<_>>()?;
        }
        if let Some(s) = shared {
            b.shared = s;
        }
        b.updated_at = Some(ts(tx.now_ms()));
        tx.update(
            &BUNDLES,
            &Row::new(row.scope.clone(), id)
                .k1(row.k1.clone().unwrap_or_default())
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&b),
        )
        .await?;
        self.record_bundle(&mut tx, org_id, &b, "agent.bundle_update")
            .await?;
        tx.commit().await?;
        Ok(b)
    }

    /// Deletes a bundle.
    pub async fn delete_bundle(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (b, row) = Self::load_bundle(&mut tx, id).await?;
        tx.delete(&BUNDLES, &row.scope, id).await?;
        tx.delete(&LOCATOR, "bundle", id).await?;
        self.record_bundle(&mut tx, org_id, &b, "agent.bundle_delete")
            .await?;
        tx.commit().await
    }

    /// Resolves a bundle at a revision (`None` = the default ref).
    pub async fn materialize_bundle(
        &self,
        cx: &Cx,
        id: &str,
        revision: Option<&str>,
        include_bodies: bool,
        max_files: u64,
    ) -> Result<MaterializeContextBundleResponse> {
        let b = self.get_bundle(cx, id).await?;
        let max_files = if max_files == 0 {
            1000
        } else {
            max_files.min(10_000)
        } as usize;
        let repo = match b.repo.as_ref() {
            Some(r) if r.repo.is_some() => Some(self.forge.orgs.resolve_repo(cx, r).await?),
            _ => None,
        };
        let at = match &repo {
            Some(r) => Some(self.ports.repo.resolve(r, revision.unwrap_or("")).await?),
            None => None,
        };
        let mut out = MaterializeContextBundleResponse {
            changeset: at.clone(),
            ..Default::default()
        };
        let mut budget = max_files;
        for it in &b.items {
            let mut ri = ContextBundleResolvedItem {
                item: Some(it.clone()),
                status: ItemStatus::Ok as i32,
                ..Default::default()
            };
            match &it.item {
                Some(context_bundle_item::Item::Path(p)) => match (&repo, &at) {
                    (Some(r), Some(at)) => {
                        let prefix = String::from_utf8_lossy(p).into_owned();
                        let (files, cut) =
                            self.ports.repo.list_files(r, at, &prefix, budget).await?;
                        if files.is_empty() {
                            ri.status = ItemStatus::NotFound as i32;
                        }
                        for f in files {
                            let size = match self.ports.repo.read_file(r, at, &f.path).await? {
                                Some((_, b)) => b.len() as u64,
                                None => f.size,
                            };
                            ri.files.push(ContextBundleResolvedFile {
                                path: f.path.into_bytes(),
                                blob: Some(f.blob),
                                size,
                            });
                        }
                        budget = budget.saturating_sub(ri.files.len());
                        if cut || budget == 0 {
                            out.truncated = true;
                        }
                    }
                    _ => ri.status = ItemStatus::NotFound as i32,
                },
                Some(context_bundle_item::Item::Subject(s)) => {
                    let srepo = match s.repo.as_ref() {
                        Some(r) if r.repo.is_some() => {
                            self.forge.orgs.resolve_repo(cx, r).await.ok()
                        }
                        _ => repo.clone(),
                    };
                    match (
                        SubjectKind::try_from(s.kind).unwrap_or(SubjectKind::Unspecified),
                        srepo,
                    ) {
                        (SubjectKind::Issue, Some(r)) => {
                            match self
                                .forge
                                .work
                                .get_issue_by_number(
                                    cx,
                                    &RepoInfo::from(&r),
                                    s.id.parse().unwrap_or(0),
                                )
                                .await
                            {
                                Ok(i) => {
                                    ri.title = i.title;
                                    if include_bodies {
                                        ri.body = i.body;
                                    }
                                }
                                Err(_) => ri.status = ItemStatus::NotFound as i32,
                            }
                        }
                        (SubjectKind::ChangeRequest, Some(r)) => {
                            let cr = match s.id.parse::<u64>() {
                                Ok(n) => self.changes.get_by_number(cx, &r, n).await,
                                Err(_) => self.changes.get(cx, &r, &s.id).await,
                            };
                            match cr {
                                Ok(c) => {
                                    ri.title = c.title;
                                    if include_bodies {
                                        ri.body = c.body;
                                    }
                                }
                                Err(_) => ri.status = ItemStatus::NotFound as i32,
                            }
                        }
                        _ => ri.status = ItemStatus::NotFound as i32,
                    }
                }
                Some(context_bundle_item::Item::Note(n)) => {
                    ri.title = it.label.clone();
                    if include_bodies {
                        ri.body = n.clone();
                    }
                }
                Some(context_bundle_item::Item::Url(_)) => ri.title = it.label.clone(),
                None => ri.status = ItemStatus::NotFound as i32,
            }
            out.items.push(ri);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_sources() {
        let (name, apply, body) =
            front_matter("---\nname: reviewer\napplyTo: \"src/**, docs/*.md\"\n---\nBe strict.\n");
        assert_eq!(name, "reviewer");
        assert_eq!(apply, vec!["src/**".to_string(), "docs/*.md".to_string()]);
        assert_eq!(body, "Be strict.\n");
        let (_, list, _) = front_matter("---\napplyTo:\n  - a/**\n  - b\n---\nx");
        assert_eq!(list, vec!["a/**".to_string(), "b".to_string()]);
        assert_eq!(front_matter("plain").2, "plain");
        assert_eq!(instruction_source("AGENTS.md"), Some(Src::AgentsMd));
        assert_eq!(instruction_source("src/CLAUDE.md"), Some(Src::ClaudeMd));
        assert_eq!(
            instruction_source(".github/agents/fix.md"),
            Some(Src::CustomAgent)
        );
        assert_eq!(
            instruction_source(".github/instructions/rust.instructions.md"),
            Some(Src::PathInstructions)
        );
        assert_eq!(instruction_source("README.md"), None);
        assert!(glob_match(&["src/**".into()], &["src/a/b.rs".into()]));
        assert!(!glob_match(&["docs/**".into()], &["src/a.rs".into()]));
    }

    #[test]
    fn memory_ranking_is_deterministic() {
        let m = |k: &str, v: &str, tags: &[&str]| AgentMemory {
            key: k.into(),
            value: v.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        };
        let a = m("build-cache", "use sccache", &["ci"]);
        let b = m("style", "prefer small diffs; build often", &[]);
        assert!(memory_score(&a, "build") > memory_score(&b, "build"));
        assert_eq!(memory_score(&a, "unrelated"), 0.0);
        assert!(memory_score(&a, "ci") > 0.0);
    }
}
