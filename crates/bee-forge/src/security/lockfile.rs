//! [`TextLockRefresher`]: rewrites the entries of updated packages in lockfiles in place.
//!
//! It handles `package-lock.json` / `npm-shrinkwrap.json` (lockfile versions 1 to 3),
//! `Cargo.lock`, `go.sum` and hashed `requirements*.txt`. Versions and tarball URLs are
//! rewritten exactly. Integrity values (npm `integrity`, Cargo `checksum`, Go hashes, pip
//! `--hash`) come from the [`Registry`] port; when the registry does not know the new version
//! the stale value is removed (npm, Cargo, pip) or the file is left alone (go.sum), so a lock
//! never carries a hash of the old version. The change request body lists the tool commands
//! (`cargo update -p ... --precise`, `go mod tidy`) a runner can use to finish what text edits
//! cannot (computing integrity from the registry).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bee_deps::update::PlannedUpdate;
use bee_deps::{Ecosystem, SourceFile};

use super::ports::{LockRefresher, Registry};
use crate::error::Result;

/// A lockfile refresher that edits text in place.
pub struct TextLockRefresher {
    registry: Arc<dyn Registry>,
}

impl TextLockRefresher {
    /// A refresher using `registry` for integrity values.
    pub fn new(registry: Arc<dyn Registry>) -> TextLockRefresher {
        TextLockRefresher { registry }
    }
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

#[async_trait]
impl LockRefresher for TextLockRefresher {
    async fn refresh(
        &self,
        files: &[SourceFile],
        updates: &[PlannedUpdate],
    ) -> Result<Vec<SourceFile>> {
        let mut current: BTreeMap<String, String> = files
            .iter()
            .map(|f| (f.path.clone(), f.content.clone()))
            .collect();
        let originals = current.clone();
        for u in updates {
            let dir = dir_of(&u.source_path).to_string();
            match u.ecosystem {
                Ecosystem::Npm => {
                    let lock = ["package-lock.json", "npm-shrinkwrap.json"]
                        .iter()
                        .map(|n| join(&dir, n))
                        .find(|p| current.contains_key(p));
                    if let Some(lock) = lock {
                        let integrity = self
                            .registry
                            .integrity(Ecosystem::Npm, &u.package, &u.to)
                            .await;
                        let reqs: Vec<(String, String)> = u
                            .edits
                            .iter()
                            .filter(|e| e.path.ends_with("package.json"))
                            .map(|e| (e.old.clone(), e.new.clone()))
                            .collect();
                        if let Some(text) = current.get_mut(&lock) {
                            *text = edit_package_lock(
                                text,
                                &u.package,
                                &u.from,
                                &u.to,
                                &reqs,
                                integrity.as_deref(),
                            );
                        }
                    }
                }
                Ecosystem::Cargo => {
                    let lock = join(&dir, "Cargo.lock");
                    // A workspace's lock sits at the root: look upward.
                    let candidates = lock_candidates(&dir, "Cargo.lock");
                    let found = candidates.into_iter().find(|p| current.contains_key(p));
                    let _ = lock;
                    if let Some(lock) = found {
                        let checksum = self
                            .registry
                            .integrity(Ecosystem::Cargo, &u.package, &u.to)
                            .await;
                        if let Some(text) = current.get_mut(&lock) {
                            *text = edit_cargo_lock(
                                text,
                                &u.package,
                                &u.from,
                                &u.to,
                                checksum.as_deref(),
                            );
                        }
                    }
                }
                Ecosystem::Go => {
                    let sum = join(&dir, "go.sum");
                    if current.contains_key(&sum) {
                        let h = self
                            .registry
                            .integrity(Ecosystem::Go, &u.package, &u.to)
                            .await;
                        let hm = self
                            .registry
                            .integrity(Ecosystem::Go, &u.package, &format!("{}/go.mod", u.to))
                            .await;
                        if let (Some(h), Some(hm), Some(text)) = (h, hm, current.get_mut(&sum)) {
                            *text = edit_go_sum(text, &u.package, &u.from, &u.to, &h, &hm);
                        }
                    }
                }
                Ecosystem::PyPI => {
                    let integrity = self
                        .registry
                        .integrity(Ecosystem::PyPI, &u.package, &u.to)
                        .await;
                    let path = u.source_path.clone();
                    if let Some(text) = current.get_mut(&path)
                        && text.contains("--hash=")
                    {
                        *text = edit_requirement_hashes(text, &u.package, integrity.as_deref());
                    }
                }
                _ => {}
            }
        }
        Ok(current
            .into_iter()
            .filter(|(p, c)| originals.get(p) != Some(c))
            .map(|(path, content)| SourceFile { path, content })
            .collect())
    }
}

fn lock_candidates(dir: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut d = dir.to_string();
    loop {
        out.push(join(&d, name));
        match d.rsplit_once('/') {
            Some((p, _)) => d = p.to_string(),
            None if !d.is_empty() => d = String::new(),
            None => break,
        }
    }
    out
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Rewrites a package-lock.json: every entry of `name` at `from` becomes `to`, and the root
/// package's requirement strings follow the manifest edits.
pub fn edit_package_lock(
    text: &str,
    name: &str,
    from: &str,
    to: &str,
    requirements: &[(String, String)],
    integrity: Option<&str>,
) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut i = 0;
    let suffix_key = format!("node_modules/{name}\": {{");
    let bare_key = format!("\"{name}\": {{");
    while i < lines.len() {
        let l = lines[i].trim().to_string();
        let is_entry =
            (l.starts_with("\"node_modules/") && l.ends_with(&suffix_key)) || l == bare_key;
        let is_root = l == "\"\": {";
        if is_entry || is_root {
            let ind = indent_of(&lines[i]);
            let mut j = i + 1;
            while j < lines.len()
                && !(indent_of(&lines[j]) == ind && lines[j].trim_start().starts_with('}'))
            {
                if is_entry {
                    let t = lines[j].trim().to_string();
                    if t.starts_with("\"version\":") && t.contains(&format!("\"{from}\"")) {
                        lines[j] = lines[j].replace(&format!("\"{from}\""), &format!("\"{to}\""));
                    } else if t.starts_with("\"resolved\":") {
                        lines[j] = lines[j].replace(&format!("-{from}.tgz"), &format!("-{to}.tgz"));
                    } else if t.starts_with("\"integrity\":") {
                        match integrity {
                            Some(h) => {
                                let comma = if t.ends_with(',') { "," } else { "" };
                                let pad = " ".repeat(indent_of(&lines[j]));
                                lines[j] = format!("{pad}\"integrity\": \"{h}\"{comma}");
                            }
                            None => {
                                let had_comma = t.ends_with(',');
                                lines.remove(j);
                                if !had_comma && j > 0 {
                                    let prev =
                                        lines[j - 1].trim_end().trim_end_matches(',').to_string();
                                    lines[j - 1] = prev;
                                }
                                continue;
                            }
                        }
                    }
                } else {
                    for (old, new) in requirements {
                        let from_s = format!("\"{name}\": \"{old}\"");
                        if lines[j].contains(&from_s) {
                            lines[j] = lines[j].replace(&from_s, &format!("\"{name}\": \"{new}\""));
                        }
                    }
                }
                j += 1;
            }
            i = if is_root { i + 1 } else { j };
        } else {
            i += 1;
        }
    }
    lines.join("\n")
}

/// Rewrites a Cargo.lock: the `[[package]]` of `name` at `from` becomes `to`, references to it
/// in other packages' `dependencies` follow, and its checksum is replaced or removed.
pub fn edit_cargo_lock(
    text: &str,
    name: &str,
    from: &str,
    to: &str,
    checksum: Option<&str>,
) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[[package]]" {
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().is_empty() && lines[j].trim() != "[[package]]"
            {
                j += 1;
            }
            let is_target = lines[i + 1..j]
                .iter()
                .any(|l| l.trim() == format!("name = \"{name}\""))
                && lines[i + 1..j]
                    .iter()
                    .any(|l| l.trim() == format!("version = \"{from}\""));
            if is_target {
                let mut k = i + 1;
                let mut end = j;
                while k < end {
                    let t = lines[k].trim().to_string();
                    if t == format!("version = \"{from}\"") {
                        lines[k] = format!("version = \"{to}\"");
                    } else if t.starts_with("checksum = ") {
                        match checksum {
                            Some(c) => lines[k] = format!("checksum = \"{c}\""),
                            None => {
                                lines.remove(k);
                                end -= 1;
                                continue;
                            }
                        }
                    }
                    k += 1;
                }
                j = end;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    // References from other packages: `"name from"` or `"name from (source)"`.
    let a = format!("\"{name} {from}\"");
    let b = format!("\"{name} {from} (");
    for l in &mut lines {
        if l.contains(&a) {
            *l = l.replace(&a, &format!("\"{name} {to}\""));
        }
        if l.contains(&b) {
            *l = l.replace(&b, &format!("\"{name} {to} ("));
        }
    }
    lines.join("\n")
}

/// Rewrites go.sum: the two lines of `module` at `from` are replaced by lines for `to`.
pub fn edit_go_sum(
    text: &str,
    module: &str,
    from: &str,
    to: &str,
    hash: &str,
    mod_hash: &str,
) -> String {
    let old = format!("{module} {from} ");
    let old_mod = format!("{module} {from}/go.mod ");
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for l in text.split('\n') {
        if l.starts_with(&old) {
            if !replaced {
                out.push(format!("{module} {to} {hash}"));
                out.push(format!("{module} {to}/go.mod {mod_hash}"));
                replaced = true;
            }
        } else if l.starts_with(&old_mod) {
            continue;
        } else {
            out.push(l.to_string());
        }
    }
    if !replaced {
        return text.to_string();
    }
    out.join("\n")
}

fn norm(name: &str) -> String {
    Ecosystem::PyPI.normalize_name(name)
}

/// Replaces or removes the `--hash` lines that follow the requirement of `name`.
pub fn edit_requirement_hashes(text: &str, name: &str, integrity: Option<&str>) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let req_name: String = l
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        if !req_name.is_empty() && norm(&req_name) == norm(name) && l.contains("==") {
            // The requirement line and its `--hash` continuations.
            let mut j = i;
            let mut block: Vec<&str> = vec![lines[i]];
            while block.last().is_some_and(|b| b.trim_end().ends_with('\\')) && j + 1 < lines.len()
            {
                j += 1;
                block.push(lines[j]);
            }
            let head = block[0].trim_end().trim_end_matches('\\').trim_end();
            let head = match head.find("--hash=") {
                Some(p) => head[..p].trim_end(),
                None => head,
            };
            let hashes: Vec<String> = match integrity {
                Some(h) => h
                    .split(',')
                    .map(|x| format!("--hash={}", x.trim()))
                    .collect(),
                None => Vec::new(),
            };
            if hashes.is_empty() {
                out.push(head.to_string());
            } else {
                out.push(format!("{head} \\"));
                let n = hashes.len();
                for (k, h) in hashes.iter().enumerate() {
                    out.push(format!("    {h}{}", if k + 1 < n { " \\" } else { "" }));
                }
            }
            i = j + 1;
        } else {
            out.push(l.to_string());
            i += 1;
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_lock_v3_entry_and_root_requirement() {
        let text = r#"{
  "name": "app",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "app",
      "dependencies": {
        "lodash": "^4.17.20"
      }
    },
    "node_modules/lodash": {
      "version": "4.17.20",
      "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz",
      "integrity": "sha512-old"
    }
  }
}
"#;
        let out = edit_package_lock(
            text,
            "lodash",
            "4.17.20",
            "4.17.21",
            &[("^4.17.20".into(), "^4.17.21".into())],
            Some("sha512-new"),
        );
        assert!(out.contains("\"lodash\": \"^4.17.21\""));
        assert!(out.contains("\"version\": \"4.17.21\""));
        assert!(out.contains("lodash-4.17.21.tgz"));
        assert!(out.contains("\"integrity\": \"sha512-new\""));
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
        let dropped = edit_package_lock(text, "lodash", "4.17.20", "4.17.21", &[], None);
        assert!(!dropped.contains("integrity"));
        assert!(serde_json::from_str::<serde_json::Value>(&dropped).is_ok());
    }

    #[test]
    fn cargo_lock_entry_and_references() {
        let text = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"time 0.3.1\",\n]\n\n[[package]]\nname = \"time\"\nversion = \"0.3.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"abc\"\n";
        let out = edit_cargo_lock(text, "time", "0.3.1", "0.3.36", Some("def"));
        assert!(out.contains("version = \"0.3.36\""));
        assert!(out.contains("\"time 0.3.36\""));
        assert!(out.contains("checksum = \"def\""));
        assert!(!out.contains("abc"));
        let dropped = edit_cargo_lock(text, "time", "0.3.1", "0.3.36", None);
        assert!(!dropped.contains("checksum"));
    }

    #[test]
    fn go_sum_and_pip_hashes() {
        let sum = "example.com/m v1.0.0 h1:old=\nexample.com/m v1.0.0/go.mod h1:oldmod=\nother v1 h1:x=\n";
        let out = edit_go_sum(
            sum,
            "example.com/m",
            "v1.0.0",
            "v1.0.1",
            "h1:new=",
            "h1:newmod=",
        );
        assert!(out.contains("example.com/m v1.0.1 h1:new="));
        assert!(out.contains("example.com/m v1.0.1/go.mod h1:newmod="));
        assert!(!out.contains("v1.0.0"));
        let req =
            "flask==2.0.1 \\\n    --hash=sha256:aaa \\\n    --hash=sha256:bbb\nrequests==2.0.0\n";
        let out = edit_requirement_hashes(req, "Flask", Some("sha256:ccc"));
        assert_eq!(
            out,
            "flask==2.0.1 \\\n    --hash=sha256:ccc\nrequests==2.0.0\n"
        );
        let out = edit_requirement_hashes(req, "flask", None);
        assert_eq!(out, "flask==2.0.1\nrequests==2.0.0\n");
    }
}
