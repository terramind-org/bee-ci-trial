//! Go: `go.mod` and `go.sum`.

use std::collections::{BTreeMap, BTreeSet};

use super::build::{self, Raw, Root};
use super::{In, Kind, Out, dir_of};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};

struct Require {
    module: String,
    version: String,
    indirect: bool,
}

struct Replace {
    old: String,
    old_version: Option<String>,
    new: String,
    new_version: Option<String>,
}

fn unquote(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| t.strip_prefix('`').and_then(|x| x.strip_suffix('`')))
        .unwrap_or(t)
}

fn split_comment(line: &str) -> (&str, &str) {
    match line.find("//") {
        Some(i) => (&line[..i], &line[i + 2..]),
        None => (line, ""),
    }
}

fn parse_go_mod(path: &str, text: &str) -> Result<(Vec<Require>, Vec<Replace>)> {
    let mut requires = Vec::new();
    let mut replaces = Vec::new();
    let mut block: Option<String> = None;
    for (n, line) in text.lines().enumerate() {
        let (code, comment) = split_comment(line);
        let code = code.trim();
        if code.is_empty() {
            continue;
        }
        let (verb, body) = match &block {
            Some(b) => {
                if code == ")" {
                    block = None;
                    continue;
                }
                (b.clone(), code)
            }
            None => {
                let (v, rest) = code.split_once(char::is_whitespace).unwrap_or((code, ""));
                let rest = rest.trim();
                if rest == "(" {
                    block = Some(v.to_owned());
                    continue;
                }
                (v.to_owned(), rest)
            }
        };
        let toks: Vec<&str> = body.split_whitespace().map(unquote).collect();
        match verb.as_str() {
            "require" => {
                if toks.len() != 2 {
                    return Err(DepsError::invalid(
                        path,
                        format!("line {}: malformed require", n + 1),
                    ));
                }
                requires.push(Require {
                    module: toks[0].to_owned(),
                    version: toks[1].to_owned(),
                    indirect: comment.split_whitespace().next() == Some("indirect"),
                });
            }
            "replace" => {
                let arrow = toks.iter().position(|t| *t == "=>").ok_or_else(|| {
                    DepsError::invalid(path, format!("line {}: replace without =>", n + 1))
                })?;
                let (l, r) = (&toks[..arrow], &toks[arrow + 1..]);
                if l.is_empty() || r.is_empty() {
                    return Err(DepsError::invalid(
                        path,
                        format!("line {}: malformed replace", n + 1),
                    ));
                }
                replaces.push(Replace {
                    old: l[0].to_owned(),
                    old_version: l.get(1).map(|s| (*s).to_owned()),
                    new: r[0].to_owned(),
                    new_version: r.get(1).map(|s| (*s).to_owned()),
                });
            }
            _ => {}
        }
    }
    if block.is_some() {
        return Err(DepsError::invalid(path, "unterminated block"));
    }
    Ok((requires, replaces))
}

fn is_local_path(p: &str) -> bool {
    p.starts_with("./") || p.starts_with("../") || p.starts_with('/') || p == "." || p == ".."
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    let mut sums: BTreeMap<&str, BTreeMap<(String, String), String>> = BTreeMap::new();
    let mut sum_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::GoSum) {
        let mut m = BTreeMap::new();
        let mut distinct = BTreeSet::new();
        for line in i.content.lines() {
            let mut it = line.split_whitespace();
            let (Some(module), Some(ver), Some(hash)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            if ver.ends_with("/go.mod") {
                continue;
            }
            distinct.insert((module, ver));
            m.insert((module.to_owned(), ver.to_owned()), hash.to_owned());
        }
        sum_counts.insert(i.path, distinct.len());
        sums.insert(dir_of(i.path), m);
    }
    let mod_dirs: BTreeSet<&str> = inputs
        .iter()
        .filter(|i| i.kind == Kind::GoMod)
        .map(|i| dir_of(i.path))
        .collect();
    for i in inputs.iter().filter(|i| i.kind == Kind::GoSum) {
        out.file(i, sum_counts.get(i.path).copied().unwrap_or(0));
        let _ = mod_dirs.contains(dir_of(i.path));
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::GoMod) {
        let (requires, replaces) = match parse_go_mod(i.path, i.content) {
            Ok(r) => r,
            Err(e) => {
                out.failed(i, &e);
                continue;
            }
        };
        let sum = sums.get(dir_of(i.path));
        let mut raw = Vec::new();
        let mut roots = Vec::new();
        for r in requires {
            let mut module = r.module.clone();
            let mut version = r.version.clone();
            // The most specific replace wins: one naming the version beats one that does not.
            let rep = replaces
                .iter()
                .find(|x| x.old == r.module && x.old_version.as_deref() == Some(r.version.as_str()))
                .or_else(|| {
                    replaces
                        .iter()
                        .find(|x| x.old == r.module && x.old_version.is_none())
                });
            if let Some(rep) = rep {
                if is_local_path(&rep.new) {
                    continue;
                }
                module = rep.new.clone();
                if let Some(v) = &rep.new_version {
                    version = v.clone();
                }
            }
            let mut p = Raw::new(&module, &version);
            p.integrity = sum
                .and_then(|s| s.get(&(module.clone(), version.clone())))
                .cloned();
            raw.push(p);
            if !r.indirect {
                roots.push(Root::new(&module, Some(&version), Scope::Runtime).pinned(&version));
            }
        }
        let pk = build::build(Ecosystem::Go, i.path, raw, roots, true);
        out.add(i, pk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_mod_blocks_and_replaces() {
        let text = "module example.com/app\n\ngo 1.21\n\nrequire (\n\tgithub.com/a/b v1.2.3\n\tgithub.com/c/d v0.1.0 // indirect\n)\n\nrequire github.com/e/f v2.0.0+incompatible\n\nreplace github.com/a/b => github.com/fork/b v1.2.4\nreplace github.com/c/d => ../d\n";
        let (req, rep) = parse_go_mod("go.mod", text).unwrap();
        assert_eq!(req.len(), 3);
        assert!(req[1].indirect);
        assert_eq!(rep.len(), 2);
        assert!(parse_go_mod("go.mod", "require (\n a v1\n").is_err());
        assert!(parse_go_mod("go.mod", "require a\n").is_err());
    }
}
