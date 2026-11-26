//! Parsing errors with locations, org overlay, and epoch stability.

use crate::common;
use bee_policy::{Action, Codeowners, Origin, Policy, PolicyErrorKind, RulesetMode, repo_epoch};
use common::{POLICY, human};
use proptest::prelude::*;

#[test]
fn syntax_and_schema_errors_carry_line_and_column() {
    let err = Policy::parse("version = 1\n[[access]\n").unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::Parse);
    assert_eq!(err.location.origin, Origin::Repo);
    assert_eq!(err.location.line, Some(2));

    let err = Policy::parse(
        "version = 1\n\n[[access]]\npath = \"/\"\nread = []\nwrite = []\nwirte = []\n",
    )
    .unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::Parse);
    assert!(err.message.contains("wirte"), "{}", err.message);
    assert_eq!(err.location.line, Some(7));

    let err = Policy::parse("version = 2\n").unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::UnsupportedVersion);
    assert_eq!(
        (err.location.line, err.location.column),
        (Some(1), Some(11))
    );
    assert_eq!(
        Policy::parse("").unwrap_err().kind,
        PolicyErrorKind::UnsupportedVersion
    );

    let src = "version = 1\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n[[refs]]\npattern = \"a*b\"\n";
    let err = Policy::parse(src).unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::InvalidPattern);
    assert_eq!(err.location.key, "refs[0].pattern");
    assert_eq!(err.location.line, Some(7));
    assert!(
        err.to_string().starts_with(".beehive/policy.toml:7:"),
        "{err}"
    );

    let src = "version = 1\n[[access]]\npath = \"/a//b\"\nread = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::InvalidPath
    );
    let src = "version = 1\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n[[access]]\npath = \"\"\nread = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::InvalidPath
    );
    let src = "version = 1\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n[[access]]\npath = \"//\"\nread = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::Duplicate
    );
    let src = "version = 1\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n[[land]]\nref = \"main\"\nagents_may_land_without_human = [\"typo\"]\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::InvalidValue
    );
    let src = "version = 1\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n[[land]]\nref = \"main\"\n[[land.approvals]]\ncount = 0\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::InvalidValue
    );
}

const ORG: &str = r#"
version = 1
[groups]
admins = ["root"]
[members]
admin = ["@admins"]
read = ["@everyone"]
[[access]]
path = "/"
read = ["@everyone"]
write = ["@admins"]
[paths]
sensitive = ["/.github/workflows"]
[push]
deny_extensions = ["exe"]
max_file_size = 100
[[rulesets]]
name = "org-signed"
refs = ["main"]
require_signed = true
"#;

const REPO: &str = r#"
version = 1
[groups]
core = ["alice"]
[members]
write = ["@core"]
[[access]]
path = "/"
write = ["@core", "@admins"]
[paths]
sensitive = ["/src/auth"]
[push]
max_file_size = 500
"#;

#[test]
fn org_defaults_are_overlaid_by_the_repo_file() {
    let p = Policy::from_sources(Some(ORG), Some(REPO)).unwrap();
    let alice = human("alice");
    // Repo `/` rule overrides only `write`; `read` comes from the org.
    assert!(p.check_access(&alice, Action::Commit, "src/x").allowed);
    assert!(
        p.check_access(&human("anyone"), Action::Read, "src/x")
            .allowed
    );
    // Org groups and roles survive.
    assert!(p.check_access(&human("root"), Action::Redact, "").allowed);
    // Path sets and deny lists union; scalar limits are replaced.
    assert!(p.is_sensitive(".github/workflows/ci.yml"));
    assert!(p.is_sensitive("src/auth/a.rs"));
    assert_eq!(p.doc().push.max_file_size, Some(500));
    assert!(p.doc().push.deny_extensions.contains("exe"));
    // Org rulesets apply and cannot be redefined by the repo.
    assert_eq!(p.doc().rulesets["org-signed"].mode, RulesetMode::Active);
    let bad = format!(
        "{REPO}\n[[rulesets]]\nname = \"org-signed\"\nrefs = [\"main\"]\nmode = \"disabled\"\n"
    );
    let err = Policy::from_sources(Some(ORG), Some(&bad)).unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::Duplicate);
    assert_eq!(err.location.origin, Origin::Repo);
    // The repo file alone is incomplete (no `/` read rule, unknown @admins).
    assert!(Policy::parse(REPO).is_err());
    // Errors in the org file say so.
    let err = Policy::from_sources(Some("version = 1\nbogus = 1\n"), Some(REPO)).unwrap_err();
    assert_eq!(err.location.origin, Origin::Org);
}

/// The same policy as `common::POLICY`, reformatted: different key order, list order,
/// duplicates, comments, path spelling and quoting.
const POLICY_REFORMATTED: &str = r#"
# Same policy, written differently.
codeowners = "/CODEOWNERS"
version = 1

[paths]
exclusive_lease = ["schema"]
generated = ["**/Cargo.lock", "**/Cargo.lock"]
sensitive = ["/src/auth/", ".beehive"]

[push]
max_path_length = 64
max_file_size = 1000
deny_extensions = [".EXE"]
deny_paths = ["/**/*.pem"]

[groups]
security = ['sam']
reviewers = ["bob"]
admins = ["root", "root"]
core = ["@reviewers", "alice"]

[members]
releaser = ["rel"]
read = ["@everyone"]
write = ["@security", "@agents", "@core"]
admin = ["@admins"]

[roles.releaser]
actions = ["manage_refs"]
base = "write"

[[owners]]
owners = ["@security"]
path = "src/auth/"

[[access]]
path = "/docs/"
write = ["@everyone"]

[[access]]
path = "secrets"
write = ["@security"]
read = ["@security"]

[[access]]
write = ["@admins", "@security", "@agents", "@core"]
read = ["@everyone"]
path = "/"

[[refs]]
pattern = "release/1.0"
delete = []

[[refs]]
delete = ["@admins"]
create = ["rel"]
pattern = "release/*"

[[land]]
ref = "main"
agents_may_land_without_human = ["comment_only", "test_only", "docs"]
dismiss_stale_approvals = true
required_checks = ["test", "test"]

[[land.approvals]]
count = 2
classes = ["sensitive"]

[[land.approvals]]
from = "owners"
classes = ["dependency", "logic"]
count = 1
"#;

#[test]
fn epoch_is_stable_under_formatting_changes() {
    let a = Policy::parse(POLICY).unwrap();
    let b = Policy::parse(POLICY_REFORMATTED).unwrap();
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(a.epoch(), b.epoch());
}

#[test]
fn epoch_changes_with_meaning() {
    let base = Policy::parse(POLICY).unwrap();
    let changed =
        Policy::parse(&POLICY.replace("max_file_size = 1000", "max_file_size = 1001")).unwrap();
    assert_ne!(base.epoch(), changed.epoch());
    let moved = Policy::parse(&POLICY.replace("bob", "bobby")).unwrap();
    assert_ne!(base.epoch(), moved.epoch());
    // CODEOWNERS content is part of the epoch.
    let with_co = base
        .clone()
        .with_codeowners(Codeowners::parse("* @x\n").unwrap());
    assert_ne!(base.epoch(), with_co.epoch());
    // The repo epoch folds in the revocation counter.
    assert_ne!(repo_epoch(base.epoch(), 0), repo_epoch(base.epoch(), 1));
    assert_eq!(repo_epoch(base.epoch(), 7), repo_epoch(base.epoch(), 7));
}

/// Frozen: changing the canonical form changes every epoch, so this value only changes with a
/// new canonical version.
#[test]
fn golden_epoch() {
    let p = Policy::parse(
        "version = 1\n[[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = []\n",
    )
    .unwrap();
    assert_eq!(p.epoch(), GOLDEN_MINIMAL_EPOCH);
    assert_eq!(repo_epoch(p.epoch(), 0), GOLDEN_MINIMAL_REPO_EPOCH);
}

const GOLDEN_MINIMAL_EPOCH: u64 = 13871477953655782449;
const GOLDEN_MINIMAL_REPO_EPOCH: u64 = 11630307122803859494;

fn render(groups: &[(String, Vec<String>)], writers: &[String], comment: bool) -> String {
    let mut s = String::from("version = 1\n");
    if comment {
        s.push_str("# a comment\n\n");
    }
    s.push_str("[groups]\n");
    for (name, members) in groups {
        let list: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
        s.push_str(&format!("{name} = [{}]\n", list.join(", ")));
    }
    let list: Vec<String> = writers.iter().map(|w| format!("\"{w}\"")).collect();
    s.push_str(&format!(
        "[[access]]\npath = \"/\"\nread = []\nwrite = [{}]\n",
        list.join(",")
    ));
    s
}

proptest! {
    #[test]
    fn epoch_ignores_order_duplicates_and_comments(
        members in prop::collection::vec(prop::collection::btree_set("[a-z]{1,6}", 0..5), 1..5),
        seed in any::<u64>(),
    ) {
        let groups: Vec<(String, Vec<String>)> = members
            .iter()
            .enumerate()
            .map(|(i, m)| (format!("g{i}"), m.iter().cloned().collect()))
            .collect();
        let writers: Vec<String> = (0..groups.len()).map(|i| format!("@g{i}")).collect();
        let a = Policy::parse(&render(&groups, &writers, false)).unwrap();

        // Permute every list and the group order with a seed, and duplicate entries.
        let mut shuffled = groups.clone();
        let rot = (seed as usize) % shuffled.len();
        shuffled.rotate_left(rot);
        for (k, (_, m)) in shuffled.iter_mut().enumerate() {
            if !m.is_empty() {
                let r = (seed as usize + k) % m.len();
                m.rotate_left(r);
                let first = m[0].clone();
                m.push(first);
            }
        }
        let mut w2 = writers.clone();
        w2.reverse();
        w2.push(writers[0].clone());
        let b = Policy::parse(&render(&shuffled, &w2, true)).unwrap();
        prop_assert_eq!(a.epoch(), b.epoch());
    }
}
