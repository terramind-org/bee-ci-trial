//! RubyGems: `Gemfile.lock`, with groups taken from `Gemfile` when it is present.

use std::collections::BTreeMap;

use super::build::{self, Raw, RawDep, Root};
use super::{In, Kind, Out, dir_of};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};

/// Gem name to scope, from `group` blocks and `group:` options in a Gemfile.
fn gemfile_scopes(text: &str) -> BTreeMap<String, Scope> {
    let dev_group = |g: &str| matches!(g, "development" | "test");
    let groups_in = |s: &str| -> Vec<String> {
        s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|t| !t.is_empty() && !matches!(*t, "group" | "groups" | "do"))
            .map(str::to_owned)
            .collect()
    };
    // One entry per open block; `Some` for `group ... do` blocks.
    let mut stack: Vec<Option<Vec<String>>> = Vec::new();
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        if l.starts_with("group ") && l.ends_with(" do") {
            stack.push(Some(groups_in(&l["group".len()..l.len() - 3])));
        } else if l.ends_with(" do")
            || l.starts_with("if ")
            || l.starts_with("unless ")
            || l.starts_with("case ")
        {
            stack.push(None);
        } else if l == "end" {
            stack.pop();
        } else if let Some(rest) = l.strip_prefix("gem ") {
            let name: String = rest
                .trim_start()
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| !matches!(c, '\'' | '"'))
                .collect();
            if name.is_empty() {
                continue;
            }
            let mut groups: Vec<String> = stack.iter().flatten().flatten().cloned().collect();
            if let Some(i) = rest
                .find("group:")
                .or_else(|| rest.find("groups:"))
                .or_else(|| rest.find(":group"))
            {
                let opt = &rest[i..];
                let opt = opt.split_once("=>").map_or(opt, |(_, r)| r);
                groups.extend(
                    groups_in(opt.split(',').next().unwrap_or(opt))
                        .into_iter()
                        .filter(|g| g != "groups"),
                );
            }
            let dev = !groups.is_empty() && groups.iter().all(|g| dev_group(g));
            out.insert(
                name,
                if dev {
                    Scope::Development
                } else {
                    Scope::Runtime
                },
            );
        }
    }
    out
}

/// Strips a platform suffix from a spec version (`1.2.3-x86_64-linux` becomes `1.2.3`).
fn strip_platform(v: &str) -> &str {
    match v.split_once('-') {
        Some((a, rest)) if rest.starts_with(|c: char| c.is_ascii_alphabetic()) => a,
        _ => v,
    }
}

fn parse_gemfile_lock(
    path: &str,
    text: &str,
    scopes: &BTreeMap<String, Scope>,
) -> Result<(Vec<Raw>, Vec<Root>)> {
    let mut raw: Vec<Raw> = Vec::new();
    let mut roots = Vec::new();
    let mut section = "";
    let mut in_specs = false;
    let mut local_section = false;
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        if indent == 0 {
            section = body;
            in_specs = false;
            local_section = section == "PATH";
            continue;
        }
        match section {
            "GEM" | "GIT" | "PATH" => {
                if indent == 2 {
                    in_specs = body == "specs:";
                } else if in_specs && indent == 4 && !local_section {
                    let (name, ver) = body
                        .split_once(" (")
                        .and_then(|(n, v)| v.strip_suffix(')').map(|v| (n, v)))
                        .ok_or_else(|| {
                            DepsError::invalid(path, format!("line {}: malformed spec", n + 1))
                        })?;
                    raw.push(Raw::new(name, strip_platform(ver)));
                } else if in_specs && indent == 6 && !local_section {
                    let (name, req) = match body.split_once(" (") {
                        Some((n, r)) => (n, r.strip_suffix(')')),
                        None => (body, None),
                    };
                    if let Some(last) = raw.last_mut() {
                        last.deps.push(RawDep::new(name, req));
                    }
                }
            }
            "DEPENDENCIES" if indent == 2 => {
                let body = body.trim_end_matches('!');
                let (name, req) = match body.split_once(" (") {
                    Some((n, r)) => (n, r.strip_suffix(')')),
                    None => (body, None),
                };
                let scope = scopes.get(name).copied().unwrap_or(Scope::Runtime);
                roots.push(Root::new(name, req, scope));
            }
            _ => {}
        }
    }
    Ok((raw, roots))
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    let mut scopes: BTreeMap<&str, BTreeMap<String, Scope>> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::Gemfile) {
        scopes.insert(dir_of(i.path), gemfile_scopes(i.content));
        out.file(i, 0);
    }
    let empty = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::GemfileLock) {
        let sc = scopes.get(dir_of(i.path)).unwrap_or(&empty);
        match parse_gemfile_lock(i.path, i.content, sc) {
            Ok((raw, roots)) => {
                let pk = build::build(Ecosystem::RubyGems, i.path, raw, roots, true);
                out.add(i, pk);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    // A Gemfile counts the gems it declares.
    for m in out
        .manifests
        .iter_mut()
        .filter(|m| m.path.ends_with("Gemfile") && !m.is_lockfile)
    {
        let d = dir_of(&m.path);
        m.dependency_count =
            u32::try_from(scopes.get(d).map_or(0, BTreeMap::len)).unwrap_or(u32::MAX);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemfile_groups() {
        let s = gemfile_scopes(
            "source 'https://rubygems.org'\ngem 'rails', '~> 7.0'\ngroup :development, :test do\n  gem 'rspec'\n  platforms :ruby do\n    gem 'byebug'\n  end\nend\ngem 'pry', group: :development\ngem 'puma'\n",
        );
        assert_eq!(s["rails"], Scope::Runtime);
        assert_eq!(s["rspec"], Scope::Development);
        assert_eq!(s["pry"], Scope::Development);
        assert_eq!(s["puma"], Scope::Runtime);
    }

    #[test]
    fn platform_suffix() {
        assert_eq!(strip_platform("1.14.0-x86_64-linux"), "1.14.0");
        assert_eq!(strip_platform("1.0.0"), "1.0.0");
        assert_eq!(strip_platform("7.0.0.rc1"), "7.0.0.rc1");
    }
}
