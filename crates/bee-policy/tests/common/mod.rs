//! Shared fixtures for policy tests.
#![allow(dead_code)]

use bee_policy::{
    Actor, ChangedPath, CheckResult, CheckStatus, DiffClass, LandContext, PrincipalKind, Review,
    Verdict,
};

/// A policy exercising most features.
pub const POLICY: &str = r#"
version = 1
codeowners = "CODEOWNERS"

[groups]
core = ["alice", "@reviewers"]
reviewers = ["bob"]
security = ["sam"]
admins = ["root"]

[roles.releaser]
base = "write"
actions = ["manage_refs"]

[members]
admin = ["@admins"]
write = ["@core", "@agents", "@security"]
read = ["@everyone"]
releaser = ["rel"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@core", "@agents", "@security", "@admins"]

[[access]]
path = "/secrets"
read = ["@security"]
write = ["@security"]

[[access]]
path = "/docs"
write = ["@everyone"]

[[refs]]
pattern = "release/*"
create = ["rel"]
delete = ["@admins"]

[[refs]]
pattern = "release/1.0"
delete = []

[[land]]
ref = "main"
required_checks = ["test"]
dismiss_stale_approvals = true
agents_may_land_without_human = ["docs", "test_only", "comment_only"]

[[land.approvals]]
classes = ["logic", "dependency"]
from = "owners"

[[land.approvals]]
classes = ["sensitive"]
count = 2

[[owners]]
path = "/src/auth"
owners = ["@security"]

[push]
deny_paths = ["**/*.pem"]
deny_extensions = ["exe"]
max_file_size = 1000
max_path_length = 64

[paths]
sensitive = ["/.beehive", "/src/auth"]
generated = ["**/Cargo.lock"]
exclusive_lease = ["/schema"]
"#;

pub fn human(id: &str) -> Actor {
    Actor::new(id, PrincipalKind::Human)
}

pub fn agent(id: &str) -> Actor {
    Actor::new(id, PrincipalKind::Agent)
}

pub fn change(path: &str, class: DiffClass) -> ChangedPath {
    ChangedPath {
        path: path.to_string(),
        class,
        size: Some(10),
    }
}

pub fn approve(reviewer: Actor, revision: &str) -> Review {
    Review {
        reviewer,
        revision: revision.to_string(),
        verdict: Verdict::Approve,
    }
}

pub fn passing(name: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status: CheckStatus::Success,
    }
}

pub fn ctx(chain: Vec<Actor>, ref_name: &str, changes: Vec<ChangedPath>) -> LandContext {
    LandContext {
        chain,
        ref_name: ref_name.to_string(),
        changes,
        head_revision: "c2".to_string(),
        last_pusher: None,
        reviews: Vec::new(),
        checks: vec![passing("test")],
        leases: Vec::new(),
        commits: Vec::new(),
        has_merges: false,
        required_workflows: Vec::new(),
        code_alerts: Vec::new(),
    }
}
