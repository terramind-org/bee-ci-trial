//! The prompt file index.
//!
//! Parsing `.prompt.yml` needs repo content, which is the VCS side's job. The indexer parses a
//! file when it scans a revision and registers the result with [`Models::index_prompt_file`];
//! `ListPromptFiles` and `GetPromptFile` are served from this index. The index keeps the most
//! recently registered version of each path (it is not versioned per revision).

use bee_proto::v1::{ModelMessageRole, PromptFile};

use super::{Change, Models, PROMPTS, SURFACE, record};
use crate::cx::Cx;
use crate::enterprise::common::{MAX_LIST, get_or_404, json, scan_all};
use crate::error::{ForgeError, Result};
use crate::ids::hex;
use crate::store::Row;

/// Largest `.prompt.yml` source the index accepts.
pub const MAX_RAW_BYTES: usize = 256 * 1024;

/// The path as UTF-8 (`bad_prompt_file` otherwise).
pub(crate) fn path_str(path: &[u8]) -> Result<String> {
    std::str::from_utf8(path)
        .map(str::to_string)
        .map_err(|_| ForgeError::invalid("bad_prompt_file", "prompt file paths must be UTF-8"))
}

fn bad(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_prompt_file", msg)
}

fn ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && s.len() <= 64
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// Checks a parsed prompt file the way the index requires: `.prompt.yml` path, a name, a
/// model, at least one message with a role, unique identifier-like variables and evaluators,
/// sane sampling parameters. Failures are `INVALID_ARGUMENT` / `bad_prompt_file`.
pub fn validate_prompt_file(f: &PromptFile) -> Result<()> {
    let path = path_str(&f.path)?;
    if path.starts_with('/')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        || !(path.ends_with(".prompt.yml") || path.ends_with(".prompt.yaml"))
    {
        return Err(bad(format!("{path:?} is not a .prompt.yml path")));
    }
    if f.blob.as_ref().is_none_or(|b| b.raw.is_empty()) {
        return Err(bad("blob (the content version indexed) is required"));
    }
    if f.name.trim().is_empty() || f.name.len() > 200 {
        return Err(bad("name must be 1-200 characters"));
    }
    if f.model.trim().is_empty() || f.model.len() > 200 {
        return Err(bad("model must be 1-200 characters"));
    }
    if f.messages.is_empty() {
        return Err(bad("a prompt file needs at least one message"));
    }
    for m in &f.messages {
        let role = ModelMessageRole::try_from(m.role).unwrap_or(ModelMessageRole::Unspecified);
        if role == ModelMessageRole::Unspecified {
            return Err(bad("every message needs a role"));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for v in &f.variables {
        if !ident(v) {
            return Err(bad(format!("variable {v:?} is not an identifier")));
        }
        if !seen.insert(v.as_str()) {
            return Err(bad(format!("variable {v} is declared twice")));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for e in &f.evaluators {
        if e.is_empty() || !seen.insert(e.as_str()) {
            return Err(bad("evaluators must be non-empty and unique"));
        }
    }
    if let Some(p) = &f.params {
        if p.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return Err(bad("temperature must be between 0 and 2"));
        }
        if p.top_p.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
            return Err(bad("top_p must be between 0 and 1"));
        }
    }
    if f.raw.len() > MAX_RAW_BYTES {
        return Err(ForgeError::too_large(
            "prompt_file_bytes",
            MAX_RAW_BYTES as u64,
            f.raw.len() as u64,
        ));
    }
    Ok(())
}

impl Models {
    /// Registers (or replaces) the parsed prompt file at its path in the repo's index; called
    /// by the indexer when it scans a revision.
    pub async fn index_prompt_file(
        &self,
        cx: &Cx,
        repo_id: &str,
        file: PromptFile,
    ) -> Result<PromptFile> {
        validate_prompt_file(&file)?;
        let path = path_str(&file.path)?;
        let repo = self.forge.orgs.repo_any(cx, repo_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let before = tx
            .get_msg::<PromptFile>(&PROMPTS, repo_id, &path)
            .await?
            .map(|(p, _)| json(&Self::header(p)));
        let blob = file.blob.as_ref().map(|b| hex(&b.raw)).unwrap_or_default();
        tx.put(
            &PROMPTS,
            &Row::new(repo_id, path.clone()).k1(blob).msg(&file),
        )
        .await?;
        record(
            &mut tx,
            Change {
                kind: "model.prompt_indexed",
                action: "model.prompt.index",
                org_id: &repo.org_id,
                repo_id,
                target: &path,
                before: before.as_deref(),
                after: &json(&Self::header(file.clone())),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(file)
    }

    /// Removes a path from the index (the file was deleted or no longer parses).
    pub async fn remove_prompt_file(&self, cx: &Cx, repo_id: &str, path: &[u8]) -> Result<()> {
        let path = path_str(path)?;
        let repo = self.forge.orgs.repo_any(cx, repo_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (old, _) =
            get_or_404::<PromptFile>(&mut tx, &PROMPTS, repo_id, &path, "prompt file").await?;
        tx.delete(&PROMPTS, repo_id, &path).await?;
        record(
            &mut tx,
            Change {
                kind: "model.prompt_indexed",
                action: "model.prompt.remove",
                org_id: &repo.org_id,
                repo_id,
                target: &path,
                before: Some(&json(&Self::header(old))),
                after: "{}",
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A prompt file reduced to its header: no messages, no source.
    fn header(mut p: PromptFile) -> PromptFile {
        p.messages.clear();
        p.raw.clear();
        p
    }

    /// Indexed prompt files under `path_prefix`, ordered by path, as headers (no messages or
    /// source); returns the page and the next token.
    pub async fn list_prompt_files(
        &self,
        cx: &Cx,
        repo_id: &str,
        path_prefix: &[u8],
        after: &str,
        limit: usize,
    ) -> Result<(Vec<PromptFile>, String)> {
        let prefix = path_str(path_prefix)?;
        let limit = limit.clamp(1, MAX_LIST);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut files: Vec<PromptFile> = scan_all::<PromptFile>(&mut tx, &PROMPTS, repo_id, None)
            .await?
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| String::from_utf8_lossy(&p.path).starts_with(&prefix))
            .filter(|p| after.is_empty() || String::from_utf8_lossy(&p.path).as_ref() > after)
            .collect();
        let next = if files.len() > limit {
            files.truncate(limit);
            files
                .last()
                .map(|p| String::from_utf8_lossy(&p.path).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok((files.into_iter().map(Self::header).collect(), next))
    }

    /// One indexed prompt file, with its source when `include_raw`.
    pub async fn get_prompt_file(
        &self,
        cx: &Cx,
        repo_id: &str,
        path: &[u8],
        include_raw: bool,
    ) -> Result<PromptFile> {
        let path = path_str(path)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, _) =
            get_or_404::<PromptFile>(&mut tx, &PROMPTS, repo_id, &path, "prompt file").await?;
        if !include_raw {
            p.raw.clear();
        }
        Ok(p)
    }
}
