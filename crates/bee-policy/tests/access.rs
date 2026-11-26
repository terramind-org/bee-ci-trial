//! Access, groups, roles, refs, owners and push rules.

use crate::common;
use bee_policy::{
    Action, Actor, Codeowners, OwnerRef, Policy, PolicyErrorKind, PrincipalKind, PushedFile,
    Reason, RefAction,
};
use common::{POLICY, agent, human};

fn policy() -> Policy {
    Policy::parse(POLICY).unwrap()
}

#[test]
fn prefix_matching_is_component_wise() {
    let p = policy();
    let sam = human("sam");
    let alice = human("alice");
    // `/secrets` is restricted, `/secrets2` falls back to `/`.
    assert!(!p.check_access(&alice, Action::Read, "secrets/key").allowed);
    assert!(p.check_access(&alice, Action::Read, "secrets2/key").allowed);
    assert!(p.check_access(&sam, Action::Read, "secrets/key").allowed);
    let d = p.check_access(&alice, Action::Read, "secrets");
    assert_eq!(d.rule, "access[/secrets].read");
    assert_eq!(d.reason, Some(Reason::PolicyDenied));
    // Leading slash in the request path is accepted.
    assert!(p.check_access(&alice, Action::Read, "/secrets2").allowed);
}

#[test]
fn read_and_write_rules_are_matched_separately() {
    let p = policy();
    // `/docs` sets only `write`; reads under it still use `/`.
    let reader = human("random");
    let d = p.check_access(&reader, Action::Read, "docs/a.md");
    assert!(d.allowed);
    assert_eq!(d.rule, "access[/].read");
    // Writes under /docs are open to everyone at the path level, but the role must grant
    // `commit`: a read-only human is still denied by `members`.
    let d = p.check_access(&reader, Action::Commit, "docs/a.md");
    assert!(!d.allowed);
    assert_eq!(d.rule, "members");
    let writer = agent("a1");
    let d = p.check_access(&writer, Action::Commit, "docs/a.md");
    assert!(d.allowed);
    assert_eq!(d.rule, "access[/docs].write");
}

#[test]
fn nested_groups_resolve() {
    let p = policy();
    let bob = human("bob");
    let groups = p.groups_of(&bob);
    assert!(groups.contains("reviewers"));
    assert!(groups.contains("core"), "core includes @reviewers");
    assert!(groups.contains("everyone"));
    assert!(groups.contains("humans"));
    assert!(p.check_access(&bob, Action::Commit, "src/lib.rs").allowed);
    // External groups come from the identity provider.
    let ext = Actor::new("ext", PrincipalKind::Human).with_groups(["org/security-team"]);
    assert!(p.groups_of(&ext).contains("org/security-team"));
}

#[test]
fn group_cycles_are_rejected_with_location() {
    let src = "version = 1\n[groups]\na = [\"@b\"]\nb = [\"@c\"]\nc = [\"@a\"]\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n";
    let err = Policy::parse(src).unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::GroupCycle);
    assert!(
        err.message.contains("@a -> @b -> @c -> @a"),
        "{}",
        err.message
    );
    assert_eq!(err.location.line, Some(3));
    assert_eq!(err.reason(), Reason::InvalidPolicy);
}

#[test]
fn unknown_group_and_role_are_rejected() {
    let src = "version = 1\n[[access]]\npath = \"/\"\nread = [\"@nope\"]\nwrite = []\n";
    let err = Policy::parse(src).unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::UnknownGroup);
    assert_eq!(err.location.line, Some(2));
    // External groups (with `/`) need no definition.
    let src = "version = 1\n[[access]]\npath = \"/\"\nread = [\"@org/team\"]\nwrite = []\n";
    assert!(Policy::parse(src).is_ok());
    let src =
        "version = 1\n[members]\nboss = [\"x\"]\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::UnknownRole
    );
    let src = "version = 1\n[roles.a]\nbase = \"b\"\n[roles.b]\nbase = \"a\"\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::RoleCycle
    );
    let src = "version = 1\n[roles.write]\nactions = []\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::ReservedName
    );
    let src =
        "version = 1\n[groups]\neveryone = []\n[[access]]\npath = \"/\"\nread = []\nwrite = []\n";
    assert_eq!(
        Policy::parse(src).unwrap_err().kind,
        PolicyErrorKind::ReservedName
    );
}

#[test]
fn root_rule_is_required() {
    let err = Policy::parse("version = 1\n[[access]]\npath = \"/docs\"\nread = []\nwrite = []\n")
        .unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::MissingRootRule);
    let err = Policy::parse("version = 1\n[[access]]\npath = \"/\"\nread = []\n").unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::MissingRootRule);
}

#[test]
fn builtin_and_custom_roles() {
    let p = policy();
    let root = human("root");
    for a in Action::ALL {
        assert!(p.check_access(&root, a, "src/x").allowed, "admin may {a}");
    }
    let reader = human("random");
    assert!(p.check_access(&reader, Action::Read, "src/x").allowed);
    assert!(!p.check_access(&reader, Action::Review, "src/x").allowed);
    let rel = human("rel");
    let acts = p.actions_of(&rel);
    assert!(
        acts.contains(&Action::ManageRefs),
        "custom role adds manage_refs"
    );
    assert!(acts.contains(&Action::Commit), "custom role extends write");
    assert!(!acts.contains(&Action::Redact));
}

#[test]
fn ref_rules_follow_precedence() {
    let p = policy();
    let rel = human("rel");
    let root = human("root");
    let alice = human("alice");
    // release/* create list names rel.
    assert!(p.check_ref(&rel, RefAction::Create, "release/2.0").allowed);
    assert!(
        !p.check_ref(&alice, RefAction::Create, "release/2.0")
            .allowed
    );
    // Delete: release/* allows admins, but the exact rule for release/1.0 denies everyone.
    assert!(p.check_ref(&root, RefAction::Delete, "release/2.0").allowed);
    let d = p.check_ref(&root, RefAction::Delete, "release/1.0");
    assert!(!d.allowed);
    assert_eq!(d.rule, "refs[release/1.0].delete");
    // The exact rule sets no create list, so create falls back to the role check.
    let d = p.check_ref(&rel, RefAction::Create, "release/1.0");
    assert!(d.allowed);
    assert_eq!(d.rule, "members");
    // Protected refs never take a direct update.
    let d = p.check_ref(&root, RefAction::Update, "main");
    assert!(!d.allowed);
    assert_eq!(d.rule, "protected");
    // Workspaces belong to their principal and its delegates.
    assert!(
        p.check_ref(&alice, RefAction::Update, "ws/alice/feature")
            .allowed
    );
    assert!(
        !p.check_ref(&alice, RefAction::Update, "ws/bob/feature")
            .allowed
    );
    let a1 = agent("a1");
    assert!(
        p.check_ref_chain(
            &[a1.clone(), alice.clone()],
            RefAction::Update,
            "ws/alice/x"
        )
        .allowed
    );
    assert!(!p.check_ref(&a1, RefAction::Update, "ws/alice/x").allowed);
    // Force update needs force_update when no list is set.
    assert!(
        !p.check_ref(&alice, RefAction::ForceUpdate, "green/x")
            .allowed
    );
    assert!(
        p.check_ref(&root, RefAction::ForceUpdate, "green/x")
            .allowed
    );
}

#[test]
fn delegation_chain_is_an_intersection() {
    let p = policy();
    let a1 = agent("a1");
    let sam = human("sam");
    // The agent alone cannot read /secrets; on behalf of sam it still cannot.
    assert!(
        !p.check_access_chain(&[a1.clone(), sam.clone()], Action::Read, "secrets/k")
            .allowed
    );
    // A reader delegating to an agent cannot give it write access.
    let reader = human("random");
    assert!(
        !p.check_access_chain(&[a1.clone(), reader], Action::Commit, "src/x")
            .allowed
    );
    assert!(
        p.check_access_chain(&[a1, human("alice")], Action::Commit, "src/x")
            .allowed
    );
}

#[test]
fn owners_from_policy_and_codeowners() {
    let co = Codeowners::parse("*.md @docs-lead\n/src/ @org/rust\n").unwrap();
    let p = policy().with_codeowners(co);
    let (rule, owners) = p.owners_of("src/auth/token.rs").unwrap();
    assert_eq!(rule, "owners[/src/auth]");
    assert!(owners.contains(&OwnerRef::Group("security".into())));
    let (rule, owners) = p.owners_of("src/lib.rs").unwrap();
    assert_eq!(rule, "CODEOWNERS:2");
    assert!(owners.contains(&OwnerRef::Group("org/rust".into())));
    let lead = human("u1").with_aliases(["docs-lead"]);
    let (_, owners) = p.owners_of("docs/a.md").unwrap();
    assert!(p.is_owner(&lead, &owners));
    assert!(p.owners_of("Makefile").is_none());
}

#[test]
fn push_rules() {
    let p = policy();
    let files = [
        PushedFile {
            path: "a/key.pem".into(),
            size: 1,
        },
        PushedFile {
            path: "bin/tool.EXE".into(),
            size: 1,
        },
        PushedFile {
            path: "big.bin".into(),
            size: 1001,
        },
        PushedFile {
            path: format!("{}/x", "d".repeat(70)),
            size: 1,
        },
        PushedFile {
            path: "ok.txt".into(),
            size: 1000,
        },
    ];
    let ids: Vec<String> = p.check_push(&files).into_iter().map(|v| v.id).collect();
    assert_eq!(
        ids,
        vec![
            "push:deny_path:a/key.pem".to_string(),
            "push:extension:bin/tool.EXE".to_string(),
            "push:max_file_size:big.bin".to_string(),
            format!("push:max_path_length:{}/x", "d".repeat(70)),
        ]
    );
}

#[test]
fn path_classes() {
    let p = policy();
    assert!(p.is_sensitive(".beehive/policy.toml"));
    assert!(p.is_sensitive("src/auth/x.rs"));
    assert!(!p.is_sensitive("src/authz.rs"));
    assert!(p.is_generated("crates/a/Cargo.lock"));
    assert!(p.requires_exclusive_lease("schema/v1.sql"));
}

#[test]
fn access_filter_agrees_with_check_access_chain() {
    let p = policy();
    let chains = [
        vec![],
        vec![human("alice")],
        vec![human("sam")],
        vec![human("random")],
        vec![agent("a1")],
        vec![agent("a1"), human("sam")],
        vec![agent("a1"), human("alice")],
        vec![agent("a1"), human("random")],
    ];
    let paths = [
        "",
        "/",
        "secrets",
        "secrets/key",
        "/secrets/key",
        "secrets2/key",
        "docs/a.md",
        "src/x",
        "ws/alice/x",
        "a/b/c/d.txt",
    ];
    for chain in &chains {
        for action in Action::ALL {
            let filter = p.access_filter(chain, action);
            for path in paths {
                assert_eq!(
                    filter.allows(path),
                    p.check_access_chain(chain, action, path).allowed,
                    "{chain:?} {action} {path:?}"
                );
            }
        }
    }
}
