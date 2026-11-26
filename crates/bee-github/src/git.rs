//! The git phase's local side: a bare mirror fetched with the system `git`, and small reads
//! from it. The mirror is then imported by `bee-import` (exact git ids, SPEC §17.4).
//!
//! Credentials go to git through `GIT_CONFIG_*` environment variables
//! (`http.extraHeader`), never on the command line, so they do not show up in process
//! listings. Prompts are disabled: a missing credential fails instead of hanging.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Result, io, not_found, unavailable};

/// Refspecs of the first fetch: every branch, tag and pull ref (SPEC §17.1 step 3).
pub const MIRROR_REFSPECS: [&str; 3] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/pull/*:refs/pull/*",
];

/// A bare git mirror.
#[derive(Clone, Debug)]
pub struct Mirror {
    dir: PathBuf,
    auth_header: Option<String>,
}

/// The output of a git command.
#[derive(Clone, Debug)]
pub struct GitOutput {
    /// Exit status was zero.
    pub ok: bool,
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error (lossy).
    pub stderr: String,
}

impl Mirror {
    /// Opens or initializes a bare mirror at `dir`. `auth_header` is an HTTP header line
    /// (`Authorization: Basic …`) sent with every fetch.
    pub fn open(dir: &Path, auth_header: Option<String>) -> Result<Mirror> {
        let m = Mirror {
            dir: dir.to_path_buf(),
            auth_header,
        };
        if !dir.join("HEAD").exists() {
            std::fs::create_dir_all(dir).map_err(|e| io("creating the git mirror", e))?;
            let out = m.run(&["init", "--bare", "--quiet"])?;
            if !out.ok {
                return Err(unavailable(format!("git init failed: {}", out.stderr)));
            }
            // GitHub's gc keeps unreachable objects a while; keep everything we fetched.
            m.run(&["config", "gc.auto", "0"])?;
        }
        Ok(m)
    }

    /// The mirror directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs git in the mirror.
    pub fn run(&self, args: &[&str]) -> Result<GitOutput> {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.dir)
            .args(args)
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("LC_ALL", "C");
        if let Some(h) = &self.auth_header {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", h);
        }
        let out = cmd
            .output()
            .map_err(|e| unavailable(format!("cannot run git (is it installed?): {e}")))?;
        Ok(GitOutput {
            ok: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// Fetches `refspecs` from `url`, pruning refs GitHub no longer has.
    pub fn fetch(&self, url: &str, refspecs: &[&str]) -> Result<()> {
        let mut args = vec![
            "-c",
            "protocol.version=2",
            "fetch",
            "--prune",
            "--no-tags",
            "--quiet",
            url,
        ];
        args.extend_from_slice(refspecs);
        let out = self.run(&args)?;
        if !out.ok {
            let low = out.stderr.to_ascii_lowercase();
            if low.contains("not found") || low.contains("does not appear to be a git repository") {
                return Err(not_found(format!("git fetch {url}: {}", out.stderr)));
            }
            return Err(unavailable(format!("git fetch {url}: {}", out.stderr)));
        }
        Ok(())
    }

    /// Fetches one commit by id into `local_ref`. `Ok(false)` when the remote no longer
    /// serves it.
    pub fn fetch_commit(&self, url: &str, sha: &str, local_ref: &str) -> Result<bool> {
        if self.has_commit(sha)? {
            return self.update_ref(local_ref, sha);
        }
        let spec = format!("+{sha}:{local_ref}");
        let out = self.run(&[
            "-c",
            "protocol.version=2",
            "fetch",
            "--no-tags",
            "--quiet",
            url,
            &spec,
        ])?;
        Ok(out.ok && self.has_commit(sha)?)
    }

    /// True when the mirror has the commit.
    pub fn has_commit(&self, sha: &str) -> Result<bool> {
        if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(false);
        }
        Ok(self
            .run(&["cat-file", "-e", &format!("{sha}^{{commit}}")])?
            .ok)
    }

    /// Points `name` at `sha` (when the commit is present). `Ok(false)` when absent.
    pub fn update_ref(&self, name: &str, sha: &str) -> Result<bool> {
        if !self.has_commit(sha)? {
            return Ok(false);
        }
        let out = self.run(&["update-ref", name, sha])?;
        if !out.ok {
            return Err(unavailable(format!(
                "git update-ref {name}: {}",
                out.stderr
            )));
        }
        Ok(true)
    }

    /// Sets `HEAD` to a branch.
    pub fn set_head(&self, branch: &str) -> Result<()> {
        let out = self.run(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])?;
        if !out.ok {
            return Err(unavailable(format!("git symbolic-ref: {}", out.stderr)));
        }
        Ok(())
    }

    /// Every ref: `(name, target id)`.
    pub fn refs(&self) -> Result<Vec<(String, String)>> {
        let out = self.run(&["for-each-ref", "--format=%(objectname) %(refname)"])?;
        if !out.ok {
            return Err(unavailable(format!("git for-each-ref: {}", out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (id, name) = l.split_once(' ')?;
                Some((name.to_string(), id.to_string()))
            })
            .collect())
    }

    /// The bytes of `path` at `rev`, when present.
    pub fn read_file(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let out = self.run(&["cat-file", "blob", &format!("{rev}:{path}")])?;
        Ok(out.ok.then_some(out.stdout))
    }

    /// File names directly under `dir` at `rev`.
    pub fn list_dir(&self, rev: &str, dir: &str) -> Result<Vec<String>> {
        let out = self.run(&["ls-tree", "--name-only", &format!("{rev}:{dir}")])?;
        if !out.ok {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// The object format (`sha1` or `sha256`).
    pub fn object_format(&self) -> Result<String> {
        let out = self.run(&["rev-parse", "--show-object-format"])?;
        let f = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if f.is_empty() { "sha1".into() } else { f })
    }

    /// Which of `shas` are commits in the mirror (one `git cat-file --batch-check`).
    pub fn existing_commits(&self, shas: &[String]) -> Result<std::collections::BTreeSet<String>> {
        let wanted: Vec<&String> = shas
            .iter()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect();
        let mut found = std::collections::BTreeSet::new();
        if wanted.is_empty() {
            return Ok(found);
        }
        let input: String = wanted.iter().map(|s| format!("{s}\n")).collect();
        let out = self.run_with_input(
            &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
            input.into_bytes(),
        )?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((id, kind)) = line.split_once(' ')
                && kind == "commit"
            {
                found.insert(id.to_string());
            }
        }
        Ok(found)
    }

    /// Points many refs at commits in one `git update-ref --stdin` (missing commits are
    /// skipped by the caller).
    pub fn update_refs(&self, pairs: &[(String, String)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let input: String = pairs
            .iter()
            .map(|(name, sha)| format!("update {name} {sha}\n"))
            .collect();
        let out = self.run_with_input(&["update-ref", "--stdin"], input.into_bytes())?;
        if !out.ok {
            return Err(unavailable(format!(
                "git update-ref --stdin: {}",
                out.stderr
            )));
        }
        Ok(())
    }

    /// Runs git with `input` on standard input.
    pub fn run_with_input(&self, args: &[&str], input: Vec<u8>) -> Result<GitOutput> {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&self.dir)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| unavailable(format!("cannot run git: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| unavailable("git stdin unavailable"))?;
        // Write from another thread so a full stdout pipe cannot deadlock us.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = child
            .wait_with_output()
            .map_err(|e| unavailable(format!("git {}: {e}", args.join(" "))))?;
        let _ = writer.join();
        Ok(GitOutput {
            ok: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// The pack of every object in the mirror, for upload to a remote server
    /// (`git pack-objects --all`).
    pub fn pack_all(&self) -> Result<Vec<u8>> {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&self.dir)
            .args(["pack-objects", "--all", "--revs", "--stdout", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| unavailable(format!("cannot run git pack-objects: {e}")))?;
        drop(child.stdin.take());
        let out = child
            .wait_with_output()
            .map_err(|e| unavailable(format!("git pack-objects: {e}")))?;
        if !out.status.success() {
            return Err(unavailable(format!(
                "git pack-objects: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }
}

/// The wiki clone URL for a repository clone URL (`…/api.git` → `…/api.wiki.git`).
pub fn wiki_url(clone_url: &str) -> String {
    let base = clone_url.strip_suffix(".git").unwrap_or(clone_url);
    format!("{base}.wiki.git")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_urls() {
        assert_eq!(
            wiki_url("https://github.com/a/b.git"),
            "https://github.com/a/b.wiki.git"
        );
        assert_eq!(wiki_url("/tmp/x"), "/tmp/x.wiki.git");
    }
}
