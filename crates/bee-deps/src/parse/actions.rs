//! GitHub Actions: `uses:` references in `.github/workflows/*.yml` and composite actions.
//!
//! Each distinct `owner/repo[/path]@ref` in a file is one dependency; the ref is the version
//! (a tag, branch or commit SHA). Local actions (`./...`) and `docker://` images are skipped.

use std::collections::BTreeSet;

use super::{In, Out};
use crate::model::{Ecosystem, Package};

fn clean_value(raw: &str) -> String {
    let mut v = raw.trim();
    if let Some(i) = v.find(" #") {
        v = v[..i].trim_end();
    }
    v.trim_matches(|c| c == '"' || c == '\'').to_owned()
}

/// Extracts the `(name, ref)` pairs a workflow file uses, in file order without duplicates.
pub(crate) fn uses_refs(text: &str) -> Vec<(String, String)> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let t = t.strip_prefix("- ").map_or(t, str::trim_start);
        let Some(rest) = t.strip_prefix("uses:") else {
            continue;
        };
        let v = clean_value(rest);
        if v.is_empty() || v.starts_with("./") || v.starts_with("docker://") || v.starts_with("${{")
        {
            continue;
        }
        let Some((name, reference)) = v.rsplit_once('@') else {
            continue;
        };
        if name.split('/').count() < 2 || reference.is_empty() {
            continue;
        }
        if seen.insert((name.to_owned(), reference.to_owned())) {
            out.push((name.to_owned(), reference.to_owned()));
        }
    }
    out
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    for i in inputs {
        let pk: Vec<Package> = uses_refs(i.content)
            .into_iter()
            .map(|(name, reference)| {
                let mut p = Package::new(Ecosystem::GitHubActions, &name, &reference, i.path);
                p.requirement = Some(reference);
                p
            })
            .collect();
        out.add(i, pk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_dedups_uses() {
        let yml = "jobs:\n  a:\n    uses: org/repo/.github/workflows/x.yml@main\n  b:\n    steps:\n      - uses: actions/checkout@v4\n      - uses: \"actions/setup-node@8f152de45cc393bb48ce5d89d36b731f54556e65\" # v4.0.0\n      - uses: ./local\n      - uses: docker://alpine:3\n      - name: x\n        uses: actions/checkout@v4\n";
        let r = uses_refs(yml);
        assert_eq!(
            r,
            vec![
                (
                    "org/repo/.github/workflows/x.yml".to_owned(),
                    "main".to_owned()
                ),
                ("actions/checkout".to_owned(), "v4".to_owned()),
                (
                    "actions/setup-node".to_owned(),
                    "8f152de45cc393bb48ce5d89d36b731f54556e65".to_owned()
                ),
            ]
        );
    }
}
