//! Change requests end to end over real history (`HistoryVcs` on in-memory VCS stores):
//! the store on every backend (memory, SQLite, Postgres when `BEE_TEST_PG` is set), the
//! CR lifecycle with carry-forward, suggestions, stale approvals and last-push approval,
//! auto-merge, the merge queue with stacks, revert, conflicts, templates, the repo context,
//! query filters, and the agent scenarios AS-CR-01/02/03 at the domain level.

use crate::common;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::changes::query::ListFilter;
use bee_forge::changes::store::ChangeEdit;
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{
    ChangeHooks, ChangeVcs, Changes, HistoryVcs, LandFacts, MemoryTeams, NewChange,
};
use bee_forge::checks::Report;
use bee_forge::error::{ForgeError, Result};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PolicySource};
use bee_forge::ids::{Clock, SystemClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

// ----- fixture -------------------------------------------------------------------------------

/// A policy source whose text tests can swap.
#[derive(Default)]
struct TestPolicy(RwLock<Option<String>>);

impl TestPolicy {
    fn set(&self, text: Option<&str>) {
        *self.0.write().unwrap() = text.map(str::to_string);
    }
}

#[async_trait]
impl PolicySource for TestPolicy {
    async fn policy(&self, _repo: &Repo) -> Result<Option<bee_policy::Policy>> {
        match self.0.read().unwrap().clone() {
            Some(t) => bee_policy::Policy::parse(&t)
                .map(Some)
                .map_err(|e| ForgeError::invalid("invalid_policy", e.to_string())),
            None => Ok(None),
        }
    }
}

/// The policy of the agent scenarios: agents may land docs, tests, comments and logic
/// without a human, logic needs one approval (agents count), sensitive paths need two
/// human approvals, `test` must pass, stale approvals are dismissed.
const POLICY: &str = r#"version = 1

[members]
write = ["@everyone"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@everyone"]

[[land]]
ref = "main"
required_checks = ["test"]
dismiss_stale_approvals = true
agents_may_land_without_human = ["docs", "test_only", "comment_only", "logic"]

[[land.approvals]]
classes = ["logic"]
count = 1
include_agents = true

[[land.approvals]]
classes = ["sensitive"]
count = 2

[[owners]]
path = "/src/auth"
owners = ["carol"]

[[owners]]
path = "/src/team"
owners = ["@acme/backend"]

[paths]
sensitive = ["/src/auth"]
"#;

struct World {
    forge: Arc<Forge>,
    changes: Arc<Changes>,
    vcs: Arc<HistoryVcs>,
    policy: Arc<TestPolicy>,
    teams: Arc<MemoryTeams>,
    directory: Arc<MemoryDirectory>,
    ring: bee_auth::KeyRing,
    clock: Arc<dyn Clock>,
    org: Org,
    repo: Repo,
}

const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("alice", PrincipalKind::Human),
    ("bob", PrincipalKind::Human),
    ("carol", PrincipalKind::Human),
    ("dave", PrincipalKind::Human),
    ("erin", PrincipalKind::Human),
    ("frank", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("ci", PrincipalKind::Service),
    ("agent", PrincipalKind::Agent),
    ("agent1", PrincipalKind::Agent),
    ("agent2", PrincipalKind::Agent),
];

async fn world_on(store: Arc<dyn ForgeStore>) -> World {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(clock.clone()));
    let writer = Writer::new(meta.clone(), blobs);
    let directory = Arc::new(MemoryDirectory::new());
    for (id, kind) in PEOPLE {
        directory.register(
            Principal {
                id: id.to_string(),
                kind: *kind as i32,
                display: id.to_string(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    let policy = Arc::new(TestPolicy::default());
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta, clock.clone())),
        policy: Some(policy.clone()),
        ..Hooks::default()
    };
    let (forge, _) = Forge::open(
        store,
        clock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        hooks,
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer, clock.clone()));
    let teams = Arc::new(MemoryTeams::new());
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: teams.clone(),
        },
    );
    changes.attach(&forge);
    let cx = Cx::new("owner", PrincipalKind::Human);
    let name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
    let org = forge
        .orgs
        .create_org(&cx, &name, "", directory_get(&directory, "owner").await)
        .await
        .unwrap();
    let repo = forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            NewRepo {
                id: forge.db.ids().ulid().unwrap(),
                name: "app".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    forge
        .hooks
        .provisioner
        .create_repo(&repo, &bee_forge::hooks::RepoInit::default())
        .await
        .unwrap();
    for who in ["reader", "dave", "erin", "frank"] {
        forge
            .orgs
            .set_member(
                &cx,
                &org.id,
                directory_get(&directory, who).await,
                Role::Member,
            )
            .await
            .unwrap();
    }
    for who in [
        "writer", "alice", "bob", "carol", "ci", "agent", "agent1", "agent2",
    ] {
        forge
            .orgs
            .set_member(
                &cx,
                &org.id,
                directory_get(&directory, who).await,
                Role::Member,
            )
            .await
            .unwrap();
        forge
            .orgs
            .add_collaborator(
                &cx,
                &repo.id,
                directory_get(&directory, who).await,
                RepoRole::Write,
                "",
            )
            .await
            .unwrap();
    }
    let w = World {
        forge,
        changes,
        vcs,
        policy,
        teams,
        directory,
        ring,
        clock,
        org,
        repo,
    };
    let root = w
        .commit(
            None,
            &[
                ("README.md", Some("# app\n\nhello\n")),
                (
                    "src/lib.rs",
                    Some("fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n"),
                ),
                ("src/auth/login.rs", Some("fn login() {}\n")),
                ("src/team/api.rs", Some("fn api() {}\n")),
                ("docs/guide.md", Some("guide\n")),
                (
                    "AGENTS.md",
                    Some("Run scripts/check.sh before handing off.\n"),
                ),
                ("src/AGENTS.md", Some("src rules\n")),
                (
                    ".github/pull_request_template.md",
                    Some("## What\n\n## Why\n"),
                ),
                (
                    ".github/PULL_REQUEST_TEMPLATE/bug.md",
                    Some("## Bug\n\nSteps:\n"),
                ),
            ],
            "root",
            "owner",
        )
        .await;
    w.vcs.set_ref(&w.repo, "main", &root).await.unwrap();
    w
}

async fn directory_get(d: &MemoryDirectory, id: &str) -> Principal {
    use bee_forge::hooks::PrincipalDirectory;
    d.get(id).await.unwrap()
}

async fn world() -> World {
    world_on(Arc::new(MemoryStore::new())).await
}

impl World {
    fn cx(&self, id: &str) -> Cx {
        let kind = PEOPLE
            .iter()
            .find(|(p, _)| *p == id)
            .map(|(_, k)| *k)
            .unwrap_or(PrincipalKind::Human);
        Cx::new(id, kind)
    }

    async fn commit(
        &self,
        parent: Option<&ObjectId>,
        edits: &[(&str, Option<&str>)],
        message: &str,
        who: &str,
    ) -> ObjectId {
        self.vcs
            .create_commit(
                &self.repo,
                NewCommit {
                    parent: parent.cloned(),
                    edits: edits
                        .iter()
                        .map(|(p, c)| (p.to_string(), c.map(|s| s.as_bytes().to_vec())))
                        .collect(),
                    message: message.to_string(),
                    author: who.to_string(),
                    creator: who.to_string(),
                    chain: vec![who.to_string()],
                    change_id: None,
                },
            )
            .await
            .unwrap()
    }

    async fn tip(&self) -> ObjectId {
        self.vcs
            .ref_tip(&self.repo, "main")
            .await
            .unwrap()
            .unwrap()
            .target
            .unwrap()
    }

    async fn file(&self, at: &ObjectId, path: &str) -> Option<String> {
        self.vcs
            .read_file(&self.repo, at, path)
            .await
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap())
    }

    /// Lands a commit on main directly (another change landing meanwhile).
    async fn advance_main(&self, edits: &[(&str, Option<&str>)], message: &str) -> ObjectId {
        let tip = self.tip().await;
        let c = self.commit(Some(&tip), edits, message, "owner").await;
        self.vcs.set_ref(&self.repo, "main", &c).await.unwrap();
        c
    }

    async fn open(&self, who: &str, head: &ObjectId, title: &str) -> ChangeRequest {
        self.changes
            .open(
                &self.cx(who),
                &self.repo,
                NewChange {
                    head: Some(head.clone()),
                    title: title.into(),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap()
    }

    async fn check(&self, head: &ObjectId, name: &str, status: CheckStatus) {
        self.forge
            .checks
            .report(
                &self.cx("ci"),
                &self.org.id,
                &self.repo.id,
                Report {
                    changeset: Some(head.clone()),
                    name: name.into(),
                    status,
                    ..Report::default()
                },
            )
            .await
            .unwrap();
    }

    async fn approve(&self, who: &str, cr: &ChangeRequest) -> Review {
        self.changes
            .submit_review(
                &self.cx(who),
                &self.repo,
                &cr.id,
                0,
                Verdict::Approve,
                "",
                vec![],
            )
            .await
            .unwrap()
    }

    async fn get(&self, id: &str) -> ChangeRequest {
        self.changes
            .get(&self.cx("owner"), &self.repo, id)
            .await
            .unwrap()
    }

    async fn kinds(&self) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&self.cx("owner"), &self.repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    fn token(&self, id: &str, scope: bee_auth::Scope) -> String {
        let principal = match PEOPLE.iter().find(|(p, _)| *p == id).map(|(_, k)| *k) {
            Some(PrincipalKind::Service) => bee_auth::Principal::service(id, id),
            Some(PrincipalKind::Agent) => bee_auth::Principal::agent(
                id,
                id,
                bee_auth::AgentInfo {
                    owner: bee_policy::PrincipalId::new("alice"),
                    runtime: "claude-code".into(),
                    purpose: "tests".into(),
                },
            ),
            _ => bee_auth::Principal::human(id, id),
        };
        self.ring
            .mint(
                &bee_auth::MintRequest {
                    principal,
                    on_behalf_of: vec![],
                    scope,
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }
}

fn reason_of<T: std::fmt::Debug>(r: Result<T>) -> ErrorReason {
    r.expect_err("expected an error").reason
}

fn missing_of(e: &ForgeError) -> Vec<String> {
    match e.payload.as_deref() {
        Some(Payload::PolicyDenial(d)) => d.missing.clone(),
        _ => Vec::new(),
    }
}

// ----- store conformance on every backend --------------------------------------------------------

/// The change-request store on one backend: numbering, lookups, revisions, reviews, threads,
/// comments, metadata, viewed files, close/reopen, land, events and the audit chain.
async fn store_flow(store: Arc<dyn ForgeStore>) {
    let w = world_on(store).await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[("docs/guide.md", Some("guide v2\n"))],
            "Update guide\n\nFixes #7",
            "alice",
        )
        .await;
    let a = w.open("alice", &h1, "Update guide").await;
    assert_eq!(a.number, 1);
    assert_eq!(a.state, ChangeState::Open as i32);
    assert_eq!(a.revisions.len(), 1);
    assert_eq!(a.revisions[0].diff_class, "docs");
    assert_eq!(a.linked_issues, vec![7]);
    assert_eq!(a.version, 1);
    let h2 = w
        .commit(
            Some(&tip),
            &[("src/lib.rs", Some("fn a() {}\n"))],
            "Trim",
            "bob",
        )
        .await;
    let b = w.open("bob", &h2, "Trim lib").await;
    assert_eq!(b.number, 2);
    assert_eq!(
        w.changes
            .get_by_number(&w.cx("owner"), &w.repo, 2)
            .await
            .unwrap()
            .id,
        b.id
    );
    // Metadata.
    let cx = w.cx("alice");
    w.changes
        .set_labels(
            &cx,
            &w.repo,
            &a.id,
            &["docs".into(), "bug".into(), "docs".into()],
        )
        .await
        .unwrap();
    w.changes
        .set_assignees(&cx, &w.repo, &a.id, &["bob".into()])
        .await
        .unwrap();
    w.changes
        .set_milestone(&cx, &w.repo, &a.id, "M1")
        .await
        .unwrap();
    let a2 = w
        .changes
        .link_issues(&cx, &w.repo, &a.id, &[9], &[], true)
        .await
        .unwrap();
    assert_eq!(
        a2.labels
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bug", "docs"]
    );
    assert_eq!(a2.assignees[0].id, "bob");
    assert_eq!(a2.milestone.as_ref().unwrap().id, "M1");
    assert_eq!(a2.linked_issues, vec![7, 9]);
    // Reviews, threads, comments.
    let c = w
        .changes
        .comment_on_lines(
            &w.cx("bob"),
            &w.repo,
            &a.id,
            "typo?",
            b"docs/guide.md",
            "new",
            1,
            1,
            false,
            0,
            None,
            false,
        )
        .await
        .unwrap();
    w.changes
        .reply(&w.cx("alice"), &w.repo, &c.thread_id, "fixed", None, false)
        .await
        .unwrap();
    w.changes
        .comment(&w.cx("carol"), &w.repo, &a.id, "LGTM overall", false)
        .await
        .unwrap();
    let t = w
        .changes
        .resolve_thread(&w.cx("alice"), &w.repo, &c.thread_id, true)
        .await
        .unwrap();
    assert!(t.resolved);
    assert_eq!(t.comments.len(), 2);
    let r = w.approve("bob", &a).await;
    assert!(r.counts_toward_policy);
    w.changes
        .set_viewed(&w.cx("bob"), &w.repo, &a.id, b"docs/guide.md", true)
        .await
        .unwrap();
    let v = w
        .changes
        .viewed(&w.cx("bob"), &w.repo, &a.id)
        .await
        .unwrap();
    assert_eq!(v.len(), 1);
    assert!(!v[0].changed_since);
    // Close, reopen, land.
    w.changes
        .close(&w.cx("bob"), &w.repo, &b.id, "superseded")
        .await
        .unwrap();
    assert_eq!(w.get(&b.id).await.state, ChangeState::Closed as i32);
    w.changes
        .reopen(&w.cx("bob"), &w.repo, &b.id)
        .await
        .unwrap();
    w.check(&h1, "test", CheckStatus::Passed).await;
    let (landed, resp) = w
        .changes
        .land(&w.cx("alice"), &w.repo, &a.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    assert_eq!(landed.state, ChangeState::Landed as i32);
    assert_eq!(resp.r#ref.unwrap().target.unwrap(), w.tip().await);
    assert_eq!(
        reason_of(w.changes.reopen(&w.cx("alice"), &w.repo, &a.id).await),
        ErrorReason::InvalidState
    );
    let kinds = w.kinds().await;
    for k in [
        "cr.opened",
        "cr.labels_changed",
        "cr.assignees_changed",
        "cr.milestone_changed",
        "cr.issues_linked",
        "review_thread.created",
        "review_comment.created",
        "cr_comment.created",
        "review_thread.resolved",
        "review.submitted",
        "cr.closed",
        "cr.reopened",
        "cr.landed",
    ] {
        assert!(kinds.iter().any(|x| x == k), "missing event {k}: {kinds:?}");
    }
    let scope = bee_forge::audit::org_scope(&w.org.id);
    let verify = w
        .forge
        .audit
        .verify(&w.cx("owner"), &scope, 0, 0, &[])
        .await
        .unwrap();
    assert!(verify.ok, "{verify:?}");
    let (entries, _) = w
        .forge
        .audit
        .query(&w.cx("owner"), &scope, &Default::default(), false, "", 500)
        .await
        .unwrap();
    for action in [
        "cr.open",
        "cr.comment",
        "review.submit",
        "review.comment",
        "cr.land",
        "cr.close",
    ] {
        assert!(
            entries.iter().any(|e| e.action == action),
            "missing audit {action}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_memory() {
    store_flow(Arc::new(MemoryStore::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    store_flow(Arc::new(store)).await;
    assert!(dir.path().join("forge-changes.db").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
        *s = format!("forge_changes_{}", std::process::id());
    }
    store_flow(Arc::new(
        SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap(),
    ))
    .await;
}

// ----- lifecycle --------------------------------------------------------------------------------

#[tokio::test]
async fn revisions_carry_threads_forward_or_mark_them_outdated() {
    let w = world().await;
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[(
                "src/lib.rs",
                Some("fn a() {}\nfn b() {}\nfn c() {}\nfn d() { 1 }\n"),
            )],
            "Change d",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h1, "Change d").await;
    let on = |line: u32, end: u32| {
        let w = &w;
        let id = cr.id.clone();
        async move {
            w.changes
                .comment_on_lines(
                    &w.cx("bob"),
                    &w.repo,
                    &id,
                    &format!("about lines {line}-{end}"),
                    b"src/lib.rs",
                    "new",
                    line,
                    end,
                    false,
                    0,
                    None,
                    false,
                )
                .await
                .unwrap()
        }
    };
    // fn a() and fn b(): unchanged by revision 2, they move down by one.
    let keep = on(1, 2).await;
    let edited = on(3, 3).await; // fn c(): edited by revision 2
    let file = w
        .changes
        .comment_on_lines(
            &w.cx("bob"),
            &w.repo,
            &cr.id,
            "file-level note",
            b"src/lib.rs",
            "new",
            0,
            0,
            true,
            0,
            None,
            false,
        )
        .await
        .unwrap();
    assert!(file.file_level);
    // Revision 2: a line inserted at the top, fn c() edited.
    let h2 = w
        .commit(
            Some(&h1),
            &[(
                "src/lib.rs",
                Some("use x;\nfn a() {}\nfn b() {}\nfn c() { 2 }\nfn d() { 1 }\n"),
            )],
            "Edit c",
            "alice",
        )
        .await;
    let r2 = w
        .changes
        .revise(&w.cx("alice"), &w.repo, &cr.id, &h2)
        .await
        .unwrap();
    assert_eq!(r2.revisions.len(), 2);
    assert_eq!(r2.revisions[1].head.as_ref().unwrap(), &h2);
    assert_eq!(r2.revisions[1].base, r2.revisions[0].base);
    let (threads, _) = w
        .changes
        .threads(&w.cx("bob"), &w.repo, &cr.id, None, "", 100)
        .await
        .unwrap();
    let by = |id: &str| threads.iter().find(|t| t.id == id).unwrap().clone();
    let k = by(&keep.thread_id);
    assert!(!k.outdated);
    assert_eq!((k.start_line, k.end_line, k.revision), (2, 3, 2));
    assert_eq!((k.comments[0].start_line, k.comments[0].end_line), (2, 3));
    let e = by(&edited.thread_id);
    assert!(e.outdated);
    assert_eq!(e.revision, 1);
    let f = by(&file.thread_id);
    assert!(!f.outdated && f.revision == 2);
    // Revising to the same head is a no-op.
    let same = w
        .changes
        .revise(&w.cx("alice"), &w.repo, &cr.id, &h2)
        .await
        .unwrap();
    assert_eq!(same.revisions.len(), 2);
    // "Changes since my review": bob reviewed revision 1.
    w.changes
        .submit_review(
            &w.cx("bob"),
            &w.repo,
            &cr.id,
            1,
            Verdict::Comment,
            "first pass",
            vec![],
        )
        .await
        .unwrap();
    let diff = w
        .changes
        .diff_revisions(&w.cx("bob"), &w.repo, &cr.id, 0, 0, true, 0)
        .await
        .unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, b"src/lib.rs");
    assert!(
        diff[0]
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l == "+fn c() { 2 }")
    );
    assert!(
        !diff[0]
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.contains("fn d() { 1 }") && l.starts_with('+'))
    );
    // After a rebase onto a moved main, the interdiff omits upstream changes.
    w.advance_main(&[("docs/guide.md", Some("upstream\n"))], "Upstream")
        .await;
    let r3 = w
        .changes
        .update_base(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Rebase)
        .await
        .unwrap();
    assert_eq!(r3.revisions.len(), 3);
    assert_ne!(r3.revisions[2].base, r3.revisions[1].base);
    let inter = w
        .changes
        .diff_revisions(&w.cx("bob"), &w.repo, &cr.id, 2, 3, false, 0)
        .await
        .unwrap();
    assert!(inter.is_empty(), "{inter:?}");
    let kinds = w.kinds().await;
    assert!(kinds.iter().any(|k| k == "review_thread.outdated"));
    assert_eq!(kinds.iter().filter(|k| *k == "cr.revised").count(), 2);
}

#[tokio::test]
async fn suggestions_apply_in_a_batch_as_a_new_revision() {
    let w = world().await;
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[
                (
                    "src/lib.rs",
                    Some("fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n"),
                ),
                ("README.md", Some("# app\n\nhelo\n")),
            ],
            "Add e",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h1, "Add e").await;
    let suggest = |path: &'static [u8], line: u32, text: &str| {
        let w = &w;
        let id = cr.id.clone();
        let text = text.to_string();
        async move {
            w.changes
                .comment_on_lines(
                    &w.cx("bob"),
                    &w.repo,
                    &id,
                    "suggestion",
                    path,
                    "new",
                    line,
                    line,
                    false,
                    0,
                    Some(Suggestion {
                        replacement: text,
                        ..Default::default()
                    }),
                    false,
                )
                .await
                .unwrap()
        }
    };
    let s1 = suggest(b"src/lib.rs", 5, "fn e() -> u8 { 0 }").await;
    let s2 = suggest(b"src/lib.rs", 1, "fn a() -> u8 { 1 }").await;
    let s3 = suggest(b"README.md", 3, "hello").await;
    // Suggestions anchor to the new side only.
    let bad = w
        .changes
        .comment_on_lines(
            &w.cx("bob"),
            &w.repo,
            &cr.id,
            "x",
            b"src/lib.rs",
            "old",
            1,
            1,
            false,
            0,
            Some(Suggestion {
                replacement: "y".into(),
                ..Default::default()
            }),
            false,
        )
        .await;
    assert_eq!(bad.unwrap_err().subcode, "invalid_suggestion");
    let after = w
        .changes
        .apply_suggestions(
            &w.cx("agent1"),
            &w.repo,
            &cr.id,
            &[s1.id.clone(), s2.id.clone(), s3.id.clone()],
            "",
        )
        .await
        .unwrap();
    assert_eq!(after.revisions.len(), 2);
    let head = after.revisions[1].head.clone().unwrap();
    assert_eq!(
        w.file(&head, "src/lib.rs").await.unwrap(),
        "fn a() -> u8 { 1 }\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() -> u8 { 0 }\n"
    );
    assert_eq!(
        w.file(&head, "README.md").await.unwrap(),
        "# app\n\nhello\n"
    );
    // The new changeset sits on the old head, authored by the applier with provenance.
    let info = w.vcs.commit(&w.repo, &head).await.unwrap();
    assert_eq!(info.parents, vec![h1.clone()]);
    assert_eq!(info.message, "Apply suggestions from code review");
    assert_eq!(info.author, "agent1");
    assert_eq!(after.change_ids.len(), 2);
    // The suggestions are marked applied, and cannot be applied twice.
    let (threads, _) = w
        .changes
        .threads(&w.cx("bob"), &w.repo, &cr.id, None, "", 100)
        .await
        .unwrap();
    for t in &threads {
        let s = t.comments[0].suggestion.as_ref().unwrap();
        assert!(s.applied);
        assert_eq!(s.applied_in.as_ref().unwrap(), &head);
    }
    assert_eq!(
        reason_of(
            w.changes
                .apply_suggestions(
                    &w.cx("alice"),
                    &w.repo,
                    &cr.id,
                    std::slice::from_ref(&s1.id),
                    ""
                )
                .await
        ),
        ErrorReason::InvalidState
    );
}

#[tokio::test]
async fn stale_approvals_are_dismissed_on_push_when_policy_says_so() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[("src/lib.rs", Some("fn a() {}\n"))],
            "Trim",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h1, "Trim").await;
    let review = w.approve("bob", &cr).await;
    w.check(&h1, "test", CheckStatus::Passed).await;
    assert!(
        w.changes
            .evaluate(&w.cx("alice"), &w.repo, &cr.id)
            .await
            .unwrap()
            .allowed
    );
    let h2 = w
        .commit(
            Some(&h1),
            &[("src/lib.rs", Some("fn a() { }\n"))],
            "Space",
            "alice",
        )
        .await;
    w.changes
        .revise(&w.cx("alice"), &w.repo, &cr.id, &h2)
        .await
        .unwrap();
    w.check(&h2, "test", CheckStatus::Passed).await;
    let reviews = w
        .changes
        .reviews(&w.cx("alice"), &w.repo, &cr.id)
        .await
        .unwrap();
    let r = reviews.iter().find(|r| r.id == review.id).unwrap();
    assert!(r.stale && r.dismissed && !r.counts_toward_policy);
    let ev = w
        .changes
        .evaluate(&w.cx("alice"), &w.repo, &cr.id)
        .await
        .unwrap();
    assert!(!ev.allowed);
    assert!(
        ev.missing.iter().any(|m| m.starts_with("approval")),
        "{ev:?}"
    );
    assert!(w.kinds().await.iter().any(|k| k == "review.dismissed"));
    // Without the policy flag approvals stay (but are marked stale).
    w.policy.set(Some(&POLICY.replace(
        "dismiss_stale_approvals = true",
        "dismiss_stale_approvals = false",
    )));
    w.approve("bob", &cr).await;
    let h3 = w
        .commit(
            Some(&h2),
            &[("src/lib.rs", Some("fn a() {  }\n"))],
            "More",
            "alice",
        )
        .await;
    w.changes
        .revise(&w.cx("alice"), &w.repo, &cr.id, &h3)
        .await
        .unwrap();
    let reviews = w
        .changes
        .reviews(&w.cx("alice"), &w.repo, &cr.id)
        .await
        .unwrap();
    let last = reviews.last().unwrap();
    assert!(last.stale && !last.dismissed);
}

#[tokio::test]
async fn last_push_must_be_approved_by_someone_else() {
    let w = world().await;
    w.policy.set(Some(
        r#"version = 1
[members]
write = ["@everyone"]
[[access]]
path = "/"
read = ["@everyone"]
write = ["@everyone"]
[[land]]
ref = "main"
require_last_push_approval = true
agents_may_land_without_human = ["docs", "test_only", "comment_only", "logic"]
"#,
    ));
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[("src/lib.rs", Some("fn a() {}\n"))],
            "Trim",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h1, "Trim").await;
    w.approve("carol", &cr).await;
    assert!(
        w.changes
            .evaluate(&w.cx("alice"), &w.repo, &cr.id)
            .await
            .unwrap()
            .allowed
    );
    // Bob pushes revision 2: carol's approval is of an older revision.
    let h2 = w
        .commit(
            Some(&h1),
            &[("src/lib.rs", Some("fn a() { 2 }\n"))],
            "Bob's push",
            "bob",
        )
        .await;
    w.changes
        .revise(&w.cx("bob"), &w.repo, &cr.id, &h2)
        .await
        .unwrap();
    let ev = w
        .changes
        .evaluate(&w.cx("alice"), &w.repo, &cr.id)
        .await
        .unwrap();
    assert!(!ev.allowed);
    assert!(ev.missing.iter().any(|m| m.contains("last_push")), "{ev:?}");
    // The pusher's own approval does not satisfy the rule.
    w.approve("bob", &cr).await;
    assert!(
        !w.changes
            .evaluate(&w.cx("alice"), &w.repo, &cr.id)
            .await
            .unwrap()
            .allowed
    );
    w.approve("carol", &cr).await;
    assert!(
        w.changes
            .evaluate(&w.cx("alice"), &w.repo, &cr.id)
            .await
            .unwrap()
            .allowed
    );
}

#[tokio::test]
async fn drafts_edits_summary_reactions_locking_and_comments() {
    let w = world().await;
    let tip = w.tip().await;
    let h = w
        .commit(
            Some(&tip),
            &[("docs/guide.md", Some("v2\n"))],
            "Guide",
            "alice",
        )
        .await;
    let cr = w
        .changes
        .open(
            &w.cx("alice"),
            &w.repo,
            NewChange {
                head: Some(h.clone()),
                title: String::new(),
                draft: true,
                template: "bug".into(),
                ..NewChange::default()
            },
        )
        .await
        .unwrap();
    assert!(cr.draft);
    assert_eq!(cr.title, "Guide", "empty title takes the head's subject");
    assert_eq!(
        cr.body, "## Bug\n\nSteps:\n",
        "the named template fills the body"
    );
    w.check(&h, "test", CheckStatus::Passed).await;
    assert_eq!(
        reason_of(
            w.changes
                .land(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
                .await
        ),
        ErrorReason::Draft
    );
    let ev = w
        .changes
        .evaluate(&w.cx("alice"), &w.repo, &cr.id)
        .await
        .unwrap();
    assert!(!ev.allowed && ev.missing.contains(&"ready_for_review".to_string()));
    let ready = w
        .changes
        .set_draft(&w.cx("alice"), &w.repo, &cr.id, false)
        .await
        .unwrap();
    assert!(!ready.draft);
    // Edit with a stale version fails; with the current one it works and relinks issues.
    assert_eq!(
        reason_of(
            w.changes
                .edit(
                    &w.cx("alice"),
                    &w.repo,
                    &cr.id,
                    ChangeEdit {
                        title: Some("x".into()),
                        expected_version: 1,
                        ..ChangeEdit::default()
                    },
                )
                .await
        ),
        ErrorReason::VersionMismatch
    );
    let edited = w
        .changes
        .edit(
            &w.cx("alice"),
            &w.repo,
            &cr.id,
            ChangeEdit {
                title: Some("Better guide".into()),
                body: Some("Closes #4".into()),
                expected_version: ready.version,
                ..ChangeEdit::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.title, "Better guide");
    assert_eq!(edited.linked_issues, vec![4]);
    let s = w
        .changes
        .set_summary(&w.cx("agent1"), &w.repo, &cr.id, "Rewrites the guide.")
        .await
        .unwrap();
    assert_eq!(s.summary, "Rewrites the guide.");
    assert_eq!(s.summary_author.unwrap().id, "agent1");
    let r = w
        .changes
        .react(&w.cx("bob"), &w.repo, &cr.id, ReactionContent::Rocket, true)
        .await
        .unwrap();
    assert_eq!(r.reactions[0].count, 1);
    let c = w
        .changes
        .comment(&w.cx("bob"), &w.repo, &cr.id, "nice", false)
        .await
        .unwrap();
    let groups = w
        .changes
        .react_comment(
            &w.cx("carol"),
            &w.repo,
            &cr.id,
            &c.id,
            ReactionContent::Heart,
            true,
        )
        .await
        .unwrap();
    assert!(groups[0].viewer_reacted);
    w.changes
        .set_locked(&w.cx("owner"), &w.repo, &cr.id, true)
        .await
        .unwrap();
    assert_eq!(
        reason_of(
            w.changes
                .comment(&w.cx("dave"), &w.repo, &cr.id, "hi", false)
                .await
        ),
        ErrorReason::Locked
    );
    w.changes
        .comment(&w.cx("owner"), &w.repo, &cr.id, "locked for now", true)
        .await
        .unwrap();
    let (comments, _) = w
        .changes
        .comments(&w.cx("bob"), &w.repo, &cr.id, "", 50)
        .await
        .unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(w.get(&cr.id).await.comment_count, 2);
    // Changeset (commit) comments outside the CR.
    w.changes
        .changeset_comment(&w.cx("carol"), &w.repo, &h, "why?", b"docs/guide.md", 1)
        .await
        .unwrap();
    let (cs, _) = w
        .changes
        .changeset_comments(&w.cx("carol"), &w.repo, &h, "", 10)
        .await
        .unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(
        cs[0].subject.as_ref().unwrap().kind,
        SubjectKind::Changeset as i32
    );
    let templates = w
        .changes
        .templates(&w.cx("bob"), &w.repo, "")
        .await
        .unwrap();
    let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["default", "bug"]);
    let kinds = w.kinds().await;
    for k in [
        "cr.ready_for_review",
        "cr.edited",
        "cr.summary_set",
        "cr.reacted",
        "cr.locked",
        "changeset_comment.created",
    ] {
        assert!(kinds.iter().any(|x| x == k), "missing {k}");
    }
}

#[tokio::test]
async fn review_requests_owners_and_round_robin_teams() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    w.teams.set_team(
        &w.org.id,
        "acme/backend",
        vec![
            directory_get(&w.directory, "dave").await,
            directory_get(&w.directory, "erin").await,
            directory_get(&w.directory, "frank").await,
        ],
        Some(TeamReviewAssignment {
            team_id: "acme/backend".into(),
            enabled: true,
            algorithm: TeamReviewAlgorithm::RoundRobin as i32,
            member_count: 1,
            ..Default::default()
        }),
    );
    let tip = w.tip().await;
    // Owners of src/auth (carol) are requested automatically.
    let h = w
        .commit(
            Some(&tip),
            &[("src/auth/login.rs", Some("fn login() { check() }\n"))],
            "Auth",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h, "Auth").await;
    let reviewers: Vec<&str> = cr
        .review_requests
        .iter()
        .filter_map(|r| r.reviewer.as_ref().map(|p| p.id.as_str()))
        .collect();
    assert_eq!(reviewers, vec!["carol"]);
    assert!(cr.review_requests[0].auto_assigned);
    // A team owner is expanded round robin.
    let pick = async |n: &str| {
        let h = w
            .commit(
                Some(&tip),
                &[("src/team/api.rs", Some(&format!("fn api() {{ {n} }}\n")))],
                n,
                "alice",
            )
            .await;
        let cr = w.open("alice", &h, n).await;
        cr.review_requests
            .iter()
            .filter_map(|r| r.reviewer.as_ref().map(|p| p.id.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(pick("one").await, vec!["dave"]);
    assert_eq!(pick("two").await, vec!["erin"]);
    assert_eq!(pick("three").await, vec!["frank"]);
    // Manual requests and removals; a review answers the request.
    let r = w
        .changes
        .request_reviewers(&w.cx("alice"), &w.repo, &cr.id, &["bob".into()], &[])
        .await
        .unwrap();
    assert_eq!(r.review_requests.len(), 2);
    assert_eq!(
        w.changes
            .request_reviewers(&w.cx("alice"), &w.repo, &cr.id, &["alice".into()], &[])
            .await
            .unwrap_err()
            .subcode,
        "self_review_request"
    );
    w.changes
        .submit_review(
            &w.cx("bob"),
            &w.repo,
            &cr.id,
            0,
            Verdict::Comment,
            "looking",
            vec![],
        )
        .await
        .unwrap();
    let after = w.get(&cr.id).await;
    assert!(
        after
            .review_requests
            .iter()
            .all(|q| q.reviewer.as_ref().unwrap().id != "bob")
    );
    let removed = w
        .changes
        .remove_review_requests(&w.cx("alice"), &w.repo, &cr.id, &["carol".into()], &[])
        .await
        .unwrap();
    assert!(removed.review_requests.is_empty());
    // Authors cannot approve their own CR.
    assert_eq!(
        reason_of(
            w.changes
                .submit_review(
                    &w.cx("alice"),
                    &w.repo,
                    &cr.id,
                    0,
                    Verdict::Approve,
                    "",
                    vec![]
                )
                .await
        ),
        ErrorReason::InvalidState
    );
    // Dismissing a review.
    let rv = w.approve("carol", &cr).await;
    let d = w
        .changes
        .dismiss_review(&w.cx("owner"), &w.repo, &cr.id, &rv.id, "wrong CR")
        .await
        .unwrap();
    assert!(d.dismissed && !d.counts_toward_policy);
}

// ----- landing -------------------------------------------------------------------------------------

#[tokio::test]
async fn auto_merge_fires_on_review_and_check_events() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    let h = w
        .commit(
            Some(&tip),
            &[("src/lib.rs", Some("fn a() {}\nfn b() {}\n"))],
            "Trim",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h, "Trim").await;
    let enabled = w
        .changes
        .enable_auto_merge(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Squash, "")
        .await
        .unwrap();
    assert!(enabled.auto_merge.is_some());
    assert_eq!(enabled.state, ChangeState::Open as i32);
    // The check passing alone is not enough: an approval is missing.
    w.check(&h, "test", CheckStatus::Passed).await;
    w.forge.flush().await.unwrap();
    assert_eq!(w.get(&cr.id).await.state, ChangeState::Open as i32);
    // The approval's review.submitted event fires the land.
    w.approve("bob", &cr).await;
    w.forge.flush().await.unwrap();
    let landed = w.get(&cr.id).await;
    assert_eq!(landed.state, ChangeState::Landed as i32);
    assert!(landed.auto_merge.is_none());
    // Squash: one new changeset on main.
    let tip2 = w.tip().await;
    let info = w.vcs.commit(&w.repo, &tip2).await.unwrap();
    assert_eq!(info.parents, vec![tip]);
    assert!(info.message.starts_with("Trim (#1)"));
    let kinds = w.kinds().await;
    assert!(kinds.iter().any(|k| k == "cr.auto_merge_enabled"));
    assert!(kinds.iter().any(|k| k == "cr.landed"));
    // Auto-merge can be disabled.
    let h2 = w
        .commit(
            Some(&tip2),
            &[("docs/guide.md", Some("x\n"))],
            "Docs",
            "alice",
        )
        .await;
    let cr2 = w.open("alice", &h2, "Docs").await;
    w.changes
        .enable_auto_merge(&w.cx("alice"), &w.repo, &cr2.id, LandMethod::Rebase, "")
        .await
        .unwrap();
    let off = w
        .changes
        .disable_auto_merge(&w.cx("alice"), &w.repo, &cr2.id)
        .await
        .unwrap();
    assert!(off.auto_merge.is_none());
    w.check(&h2, "test", CheckStatus::Passed).await;
    w.forge.flush().await.unwrap();
    assert_eq!(w.get(&cr2.id).await.state, ChangeState::Open as i32);
    // Repo settings can refuse disallowed methods and auto-merge altogether.
    w.forge
        .orgs
        .update_repo_settings(&w.cx("owner"), &w.repo.id, 0, |s| {
            s.allowed_land_methods = vec![LandMethod::Rebase as i32];
            s.allow_auto_merge = false;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        w.changes
            .land(&w.cx("alice"), &w.repo, &cr2.id, LandMethod::Squash, "", 0)
            .await
            .unwrap_err()
            .subcode,
        "land_method_not_allowed"
    );
    assert_eq!(
        w.changes
            .enable_auto_merge(&w.cx("alice"), &w.repo, &cr2.id, LandMethod::Rebase, "")
            .await
            .unwrap_err()
            .subcode,
        "auto_merge_disabled"
    );
}

#[tokio::test]
async fn land_methods_rebase_squash_and_merge_commit() {
    let w = world().await;
    // Rebase: a two-changeset stack lands as two rebased changesets.
    let tip = w.tip().await;
    let a = w
        .commit(Some(&tip), &[("docs/a.md", Some("a\n"))], "A", "alice")
        .await;
    let b = w
        .commit(Some(&a), &[("docs/b.md", Some("b\n"))], "B", "alice")
        .await;
    let cr = w.open("alice", &b, "AB").await;
    assert_eq!(cr.change_ids.len(), 2);
    let moved = w
        .advance_main(&[("README.md", Some("# app\n\nmoved\n"))], "Move")
        .await;
    let (_, resp) = w
        .changes
        .land(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    assert_eq!(resp.landed.len(), 2);
    let top = w.vcs.commit(&w.repo, &w.tip().await).await.unwrap();
    assert_eq!(top.message, "B");
    let below = w.vcs.commit(&w.repo, &top.parents[0]).await.unwrap();
    assert_eq!(
        (below.message.as_str(), below.parents.clone()),
        ("A", vec![moved])
    );
    // Squash: one changeset with the stack's combined delta.
    let tip = w.tip().await;
    let c = w
        .commit(Some(&tip), &[("docs/c.md", Some("c\n"))], "C", "bob")
        .await;
    let d = w
        .commit(Some(&c), &[("docs/d.md", Some("d\n"))], "D", "bob")
        .await;
    let cr2 = w.open("bob", &d, "CD").await;
    w.changes
        .land(
            &w.cx("bob"),
            &w.repo,
            &cr2.id,
            LandMethod::Squash,
            "Squashed CD",
            0,
        )
        .await
        .unwrap();
    let sq = w.vcs.commit(&w.repo, &w.tip().await).await.unwrap();
    assert_eq!(
        (sq.message.as_str(), sq.parents.clone()),
        ("Squashed CD", vec![tip])
    );
    assert!(w.file(&w.tip().await, "docs/c.md").await.is_some());
    assert!(w.file(&w.tip().await, "docs/d.md").await.is_some());
    // Merge commit: the stack head becomes the second parent.
    let tip = w.tip().await;
    let e = w
        .commit(Some(&tip), &[("docs/e.md", Some("e\n"))], "E", "carol")
        .await;
    let cr3 = w.open("carol", &e, "E").await;
    let up = w
        .advance_main(&[("docs/guide.md", Some("up\n"))], "Up")
        .await;
    w.changes
        .land(
            &w.cx("carol"),
            &w.repo,
            &cr3.id,
            LandMethod::MergeCommit,
            "",
            0,
        )
        .await
        .unwrap();
    let m = w.vcs.commit(&w.repo, &w.tip().await).await.unwrap();
    assert_eq!(m.parents, vec![up, e]);
    assert_eq!(m.message, format!("Land #{}: E", cr3.number));
    // Reverting a merge-commit land restores the tip before it.
    let rv = w
        .changes
        .revert(&w.cx("carol"), &w.repo, &cr3.id, "", "", false)
        .await
        .unwrap();
    let rv_head = rv.revisions[0].head.clone().unwrap();
    assert_eq!(w.file(&rv_head, "docs/e.md").await, None);
    assert_eq!(w.file(&rv_head, "docs/guide.md").await.unwrap(), "up\n");
}

#[tokio::test]
async fn landing_releases_leases_and_reports_closing_issues() {
    let w = world().await;
    let tip = w.tip().await;
    let h = w
        .commit(
            Some(&tip),
            &[("src/lib.rs", Some("fn z() {}\n"))],
            "Replace\n\nResolves #12",
            "agent",
        )
        .await;
    let cr = w.open("agent", &h, "Replace").await;
    let (lease, _) = w
        .forge
        .leases
        .acquire(
            &w.cx("agent"),
            bee_forge::leases::AcquireRequest {
                repo_id: w.repo.id.clone(),
                org_id: w.org.id.clone(),
                ref_name: "main".into(),
                paths: vec![b"src".to_vec()],
                mode: LeaseMode::Exclusive,
                change_id: cr.change_ids.first().cloned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Another principal's exclusive lease blocks the land with LEASED.
    let h2 = w
        .commit(
            Some(&tip),
            &[("src/lib.rs", Some("fn y() {}\n"))],
            "Other",
            "bob",
        )
        .await;
    let other = w.open("bob", &h2, "Other").await;
    let e = w
        .changes
        .land(&w.cx("bob"), &w.repo, &other.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::Leased, "{e}");
    w.changes
        .land(&w.cx("agent"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    w.forge.flush().await.unwrap();
    assert!(
        w.forge
            .leases
            .get(&w.cx("agent"), &w.repo.id, &lease.id)
            .await
            .is_err(),
        "cr.landed released the CR's lease"
    );
    let (events, _) = w
        .forge
        .events
        .read(&w.cx("owner"), &w.repo.id, 0, &["cr.landed".into()], 10)
        .await
        .unwrap();
    let ev = &events[0];
    let Some(event::Body::Change(body)) = &ev.body else {
        panic!("cr.landed carries the ChangeRequest")
    };
    assert_eq!(body.change_ids, cr.change_ids);
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["closes_issues"], serde_json::json!([12]));
}

#[tokio::test]
async fn revert_opens_a_cr_that_undoes_a_landed_cr() {
    let w = world().await;
    let tip = w.tip().await;
    let h = w
        .commit(
            Some(&tip),
            &[
                ("README.md", Some("# app\n\nchanged\n")),
                ("NEW.md", Some("new\n")),
            ],
            "Change readme",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h, "Change readme").await;
    w.changes
        .land(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    // Main moves on before the revert.
    w.advance_main(&[("docs/guide.md", Some("later\n"))], "Later")
        .await;
    assert_eq!(
        reason_of(
            w.changes
                .revert(
                    &w.cx("bob"),
                    &w.repo,
                    &w.open(
                        "bob",
                        &w.commit(Some(&w.tip().await), &[("x.md", Some("x\n"))], "x", "bob")
                            .await,
                        "x"
                    )
                    .await
                    .id,
                    "",
                    "",
                    false
                )
                .await
        ),
        ErrorReason::InvalidState
    );
    let rv = w
        .changes
        .revert(&w.cx("bob"), &w.repo, &cr.id, "", "", false)
        .await
        .unwrap();
    assert_eq!(rv.revert_of, cr.id);
    assert_eq!(rv.title, "Revert \"Change readme\"");
    assert_eq!(rv.body, format!("Reverts #{}", cr.number));
    w.changes
        .land(&w.cx("bob"), &w.repo, &rv.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    let t = w.tip().await;
    assert_eq!(w.file(&t, "README.md").await.unwrap(), "# app\n\nhello\n");
    assert_eq!(w.file(&t, "NEW.md").await, None);
    assert_eq!(w.file(&t, "docs/guide.md").await.unwrap(), "later\n");
}

#[tokio::test]
async fn conflicts_are_held_for_a_resolver() {
    let w = world().await;
    let tip = w.tip().await;
    let h = w
        .commit(
            Some(&tip),
            &[("README.md", Some("# app\n\nours\n"))],
            "Ours",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h, "Ours").await;
    w.advance_main(&[("README.md", Some("# app\n\ntheirs\n"))], "Theirs")
        .await;
    let ev = w
        .changes
        .evaluate(&w.cx("alice"), &w.repo, &cr.id)
        .await
        .unwrap();
    assert!(!ev.mergeable);
    let e = w
        .changes
        .land(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::MergeConflict);
    assert!(
        matches!(e.payload.as_deref(), Some(Payload::Conflicts(c)) if c.conflicts[0].path == b"README.md")
    );
    let (open, _) = w
        .changes
        .conflicts(&w.cx("alice"), &w.repo, LandConflictState::Open, "", 10)
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(
        reason_of(
            w.changes
                .update_base(&w.cx("alice"), &w.repo, &cr.id, LandMethod::Rebase)
                .await
        ),
        ErrorReason::MergeConflict
    );
    // The resolver rebases by hand and submits the result, asking to land it.
    let resolved = w
        .commit(
            Some(&w.tip().await),
            &[("README.md", Some("# app\n\nours and theirs\n"))],
            "Ours (resolved)",
            "agent2",
        )
        .await;
    let c = w
        .changes
        .submit_resolution(&w.cx("agent2"), &w.repo, &open[0].id, &resolved, true)
        .await
        .unwrap();
    assert_eq!(c.resolution.as_ref().unwrap(), &resolved);
    let landed = w.get(&cr.id).await;
    assert_eq!(landed.state, ChangeState::Landed as i32, "{c:?}");
    assert_eq!(
        w.changes
            .conflict(&w.cx("alice"), &w.repo, &open[0].id)
            .await
            .unwrap()
            .state,
        LandConflictState::Resolved as i32
    );
    assert!(
        w.kinds()
            .await
            .iter()
            .any(|k| k == "land_conflict.resolved")
    );
}

#[tokio::test]
async fn update_base_by_merge_commit() {
    let w = world().await;
    let tip = w.tip().await;
    let h = w
        .commit(
            Some(&tip),
            &[("docs/guide.md", Some("mine\n"))],
            "Mine",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h, "Mine").await;
    let up = w
        .advance_main(&[("README.md", Some("# app\n\nupstream\n"))], "Upstream")
        .await;
    let r = w
        .changes
        .update_base(&w.cx("alice"), &w.repo, &cr.id, LandMethod::MergeCommit)
        .await
        .unwrap();
    let head = r.revisions.last().unwrap().head.clone().unwrap();
    let info = w.vcs.commit(&w.repo, &head).await.unwrap();
    assert_eq!(info.parents, vec![h, up]);
    assert_eq!(
        w.file(&head, "README.md").await.unwrap(),
        "# app\n\nupstream\n"
    );
}

#[tokio::test]
async fn repo_context_names_owners_leases_crs_checks_rules_and_instructions() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    w.check(&tip, "test", CheckStatus::Failed).await;
    let h = w
        .commit(
            Some(&tip),
            &[("src/auth/login.rs", Some("fn login() { 2 }\n"))],
            "Auth",
            "alice",
        )
        .await;
    let cr = w.open("alice", &h, "Auth").await;
    w.forge
        .leases
        .acquire(
            &w.cx("bob"),
            bee_forge::leases::AcquireRequest {
                repo_id: w.repo.id.clone(),
                org_id: w.org.id.clone(),
                ref_name: "main".into(),
                paths: vec![b"src/auth".to_vec()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let ctx = w
        .changes
        .context(
            &w.cx("agent"),
            &w.repo,
            "",
            &[b"src/auth/login.rs".to_vec(), b"docs".to_vec()],
        )
        .await
        .unwrap();
    assert_eq!(ctx.owners[0].owners, vec!["carol".to_string()]);
    assert!(ctx.owners[1].owners.is_empty());
    assert_eq!(ctx.active_leases.len(), 1);
    assert_eq!(ctx.open_changes_touching.len(), 1);
    assert_eq!(ctx.open_changes_touching[0].id, cr.id);
    assert_eq!(ctx.failing_checks.len(), 1);
    assert!(
        ctx.land_rules
            .iter()
            .any(|r| r.contains("required checks: test"))
    );
    let paths: Vec<String> = ctx
        .instructions
        .iter()
        .map(|i| String::from_utf8(i.path.clone()).unwrap())
        .collect();
    assert_eq!(paths, vec!["AGENTS.md", "src/AGENTS.md"]);
    assert_eq!(ctx.sensitive_paths, vec!["src/auth/login.rs".to_string()]);
    assert!(!ctx.policy_epoch.is_empty());
}

#[tokio::test]
async fn list_filters_and_the_query_language() {
    let w = world().await;
    let tip = w.tip().await;
    let mk = async |who: &str, path: &str, title: &str| {
        let h = w
            .commit(
                Some(&tip),
                &[(path, Some(&format!("{title}\n")))],
                title,
                who,
            )
            .await;
        w.open(who, &h, title).await
    };
    let a = mk("alice", "docs/a.md", "Docs A").await;
    let b = mk("bob", "src/lib.rs", "Logic B").await;
    let c = mk("agent", "docs/c.md", "Agent docs C").await;
    w.changes
        .set_labels(&w.cx("alice"), &w.repo, &a.id, &["bug".into()])
        .await
        .unwrap();
    w.changes
        .set_draft(&w.cx("bob"), &w.repo, &b.id, true)
        .await
        .unwrap();
    w.approve("bob", &a).await;
    w.changes
        .close(&w.cx("agent"), &w.repo, &c.id, "dup")
        .await
        .unwrap();
    let q = async |query: &str| -> Vec<u64> {
        let (crs, _) = w
            .changes
            .list(
                &w.cx("alice"),
                &w.repo,
                &ListFilter {
                    query: query.into(),
                    ..ListFilter::default()
                },
                "",
                50,
            )
            .await
            .unwrap();
        crs.iter().map(|c| c.number).collect()
    };
    assert_eq!(q("").await, vec![3, 2, 1]);
    assert_eq!(q("is:open").await, vec![2, 1]);
    assert_eq!(q("is:closed").await, vec![3]);
    assert_eq!(q("is:draft").await, vec![2]);
    assert_eq!(q("label:bug").await, vec![1]);
    assert_eq!(q("-label:bug is:open").await, vec![2]);
    assert_eq!(q("author:@me").await, vec![1]);
    assert_eq!(q("author:bob-login").await, vec![2]);
    assert_eq!(q("agent:true").await, vec![3]);
    assert_eq!(q("review:approved").await, vec![1]);
    assert_eq!(q("review:none").await, vec![3, 2]);
    assert_eq!(q("path:docs").await, vec![3, 1]);
    assert_eq!(q("risk:>=logic").await, vec![2]);
    assert_eq!(q("Logic in:title").await, vec![2]);
    assert_eq!(q("is:open sort:created-asc").await, vec![1, 2]);
    assert_eq!(q("is:issue").await, Vec::<u64>::new());
    assert_eq!(q("no:label is:open").await, vec![2]);
    assert!(matches!(
        w.changes
            .list(&w.cx("alice"), &w.repo, &ListFilter { query: "frobnicate:x".into(), ..ListFilter::default() }, "", 10)
            .await,
        Err(e) if e.reason == ErrorReason::InvalidArgument
    ));
    // Request-level filters and paging.
    let (page1, next) = w
        .changes
        .list(&w.cx("alice"), &w.repo, &ListFilter::default(), "", 2)
        .await
        .unwrap();
    assert_eq!(page1.len(), 2);
    let (page2, end) = w
        .changes
        .list(&w.cx("alice"), &w.repo, &ListFilter::default(), &next, 2)
        .await
        .unwrap();
    assert_eq!((page2.len(), end.as_str()), (1, ""));
    let (by_path, _) = w
        .changes
        .list(
            &w.cx("alice"),
            &w.repo,
            &ListFilter {
                path_prefix: "src".into(),
                state: ChangeState::Open,
                ..ListFilter::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        by_path.iter().map(|c| c.number).collect::<Vec<_>>(),
        vec![2]
    );
}

// ----- agent scenarios (domain level) ---------------------------------------------------------------

/// AS-CR-01: an agent opens a CR, another agent reviews it with a suggestion, the author
/// applies the suggestion, the reviewer approves, and the CR lands through auto-merge when
/// the required check passes.
#[tokio::test]
async fn as_cr_01_review_by_agent_suggestions_and_auto_merge() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[(
                "src/lib.rs",
                Some("fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n"),
            )],
            "Add e",
            "agent1",
        )
        .await;
    let cr = w.open("agent1", &h1, "Add e").await;
    assert_eq!(cr.revisions[0].diff_class, "logic");
    // agent2 requests changes with a suggestion.
    let review = w
        .changes
        .submit_review(
            &w.cx("agent2"),
            &w.repo,
            &cr.id,
            0,
            Verdict::RequestChanges,
            "e should return a value",
            vec![ReviewComment {
                path: b"src/lib.rs".to_vec(),
                side: "new".into(),
                start_line: 5,
                end_line: 5,
                body: "suggest".into(),
                suggestion: Some(Suggestion {
                    replacement: "fn e() -> u8 { 0 }".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
    assert_eq!(review.comments.len(), 1);
    let ev = w
        .changes
        .evaluate(&w.cx("agent1"), &w.repo, &cr.id)
        .await
        .unwrap();
    assert!(!ev.allowed);
    // The author applies the suggestion: a new revision.
    let revised = w
        .changes
        .apply_suggestions(
            &w.cx("agent1"),
            &w.repo,
            &cr.id,
            &[review.comments[0].id.clone()],
            "Apply review suggestion",
        )
        .await
        .unwrap();
    let h2 = revised.revisions[1].head.clone().unwrap();
    assert!(
        w.file(&h2, "src/lib.rs")
            .await
            .unwrap()
            .contains("fn e() -> u8 { 0 }")
    );
    // agent2 sees only its suggestion since its review, then approves.
    let since = w
        .changes
        .diff_revisions(&w.cx("agent2"), &w.repo, &cr.id, 0, 0, true, 0)
        .await
        .unwrap();
    assert_eq!(since.len(), 1);
    w.approve("agent2", &revised).await;
    // agent1 enables auto-merge; it fires when `test` passes on the new head.
    w.changes
        .enable_auto_merge(&w.cx("agent1"), &w.repo, &cr.id, LandMethod::Rebase, "")
        .await
        .unwrap();
    w.forge.flush().await.unwrap();
    assert_eq!(w.get(&cr.id).await.state, ChangeState::Open as i32);
    w.check(&h2, "test", CheckStatus::Passed).await;
    w.forge.flush().await.unwrap();
    let landed = w.get(&cr.id).await;
    assert_eq!(landed.state, ChangeState::Landed as i32);
    assert_eq!(landed.landed_ref.unwrap().target.unwrap(), w.tip().await);
    assert!(
        w.file(&w.tip().await, "src/lib.rs")
            .await
            .unwrap()
            .contains("fn e() -> u8 { 0 }")
    );
}

/// AS-CR-02: an agent lands a docs-only change without a human approval and is refused
/// when the change touches a sensitive path.
#[tokio::test]
async fn as_cr_02_docs_without_human_sensitive_refused() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    let docs = w
        .commit(
            Some(&tip),
            &[("docs/guide.md", Some("better guide\n"))],
            "Docs",
            "agent",
        )
        .await;
    let cr = w.open("agent", &docs, "Docs").await;
    assert_eq!(cr.revisions[0].diff_class, "docs");
    w.check(&docs, "test", CheckStatus::Passed).await;
    let ev = w
        .changes
        .evaluate(&w.cx("agent"), &w.repo, &cr.id)
        .await
        .unwrap();
    assert!(ev.allowed, "{ev:?}");
    let (landed, _) = w
        .changes
        .land(&w.cx("agent"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    assert_eq!(landed.state, ChangeState::Landed as i32);
    // The same agent touching src/auth is refused: a human must approve (twice).
    let tip = w.tip().await;
    let auth = w
        .commit(
            Some(&tip),
            &[
                ("src/auth/login.rs", Some("fn login() { bypass() }\n")),
                ("docs/auth.md", Some("x\n")),
            ],
            "Auth",
            "agent",
        )
        .await;
    let cr2 = w.open("agent", &auth, "Auth").await;
    assert_eq!(cr2.revisions[0].diff_class, "sensitive");
    w.check(&auth, "test", CheckStatus::Passed).await;
    w.approve("agent2", &cr2).await;
    let e = w
        .changes
        .land(&w.cx("agent"), &w.repo, &cr2.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::PolicyDenied);
    let missing = missing_of(&e);
    assert!(missing.iter().any(|m| m == "approval:human"), "{missing:?}");
    let ev = w
        .changes
        .evaluate(&w.cx("agent"), &w.repo, &cr2.id)
        .await
        .unwrap();
    assert!(!ev.allowed);
    assert_eq!(ev.diff_class, "sensitive");
    // Two human approvals satisfy it.
    w.approve("carol", &cr2).await;
    w.approve("bob", &cr2).await;
    assert!(
        w.changes
            .evaluate(&w.cx("agent"), &w.repo, &cr2.id)
            .await
            .unwrap()
            .allowed
    );
}

/// Land facts of a server whose only active merge-queue ruleset matches `main` (SF4).
struct MainQueueRuleset;

#[async_trait]
impl LandFacts for MainQueueRuleset {
    async fn signatures(
        &self,
        _repo: &Repo,
        stack: &[ObjectId],
    ) -> Result<Vec<bee_policy::SignatureState>> {
        Ok(vec![bee_policy::SignatureState::Missing; stack.len()])
    }
    async fn required_workflows(
        &self,
        _repo: &Repo,
        _heads: &[ObjectId],
    ) -> Result<Vec<bee_policy::WorkflowGate>> {
        Ok(Vec::new())
    }
    async fn code_alerts(
        &self,
        _repo: &Repo,
        _target_ref: &str,
        _heads: &[ObjectId],
    ) -> Result<Vec<bee_policy::CodeAlert>> {
        Ok(Vec::new())
    }
    async fn merge_queue_required(&self, _repo: &Repo, target_ref: &str) -> Result<bool> {
        Ok(target_ref == "main")
    }
}

/// A merge-queue ruleset on the target ref works like the repo setting: a direct land is
/// refused with `merge_queue_required`, auto-merge enqueues, and the queue lands.
#[tokio::test]
async fn a_merge_queue_ruleset_refuses_direct_lands_and_auto_merge_enqueues() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[("src/one.rs", Some("fn one() {}\n"))],
            "One",
            "alice",
        )
        .await;
    let cr1 = w.open("alice", &h1, "One").await;
    w.approve("bob", &cr1).await;
    w.check(&h1, "test", CheckStatus::Passed).await;
    assert!(
        w.changes
            .evaluate(&w.cx("alice"), &w.repo, &cr1.id)
            .await
            .unwrap()
            .allowed,
        "policy alone would let it land"
    );
    w.changes.set_land_facts(Arc::new(MainQueueRuleset));
    let e = w
        .changes
        .land(&w.cx("alice"), &w.repo, &cr1.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);
    assert_eq!(e.subcode, "merge_queue_required");
    // The ruleset alone requires the queue; the repo setting is off.
    assert!(
        !w.forge
            .orgs
            .repo_settings(&w.cx("owner"), &w.repo.id)
            .await
            .unwrap()
            .merge_queue_required
    );
    assert!(
        w.changes
            .queue_required(&w.cx("alice"), &w.repo, "main")
            .await
            .unwrap()
    );
    assert!(
        !w.changes
            .queue_required(&w.cx("alice"), &w.repo, "release/1")
            .await
            .unwrap()
    );

    // Auto-merge enqueues once policy allows, instead of landing.
    let h2 = w
        .commit(
            Some(&tip),
            &[("docs/two.md", Some("two\n"))],
            "Two",
            "alice",
        )
        .await;
    let cr2 = w.open("alice", &h2, "Two").await;
    w.changes
        .enable_auto_merge(&w.cx("alice"), &w.repo, &cr2.id, LandMethod::Rebase, "")
        .await
        .unwrap();
    w.check(&h2, "test", CheckStatus::Passed).await;
    w.approve("bob", &cr2).await;
    w.forge.flush().await.unwrap();
    let queued = w.get(&cr2.id).await;
    assert_eq!(queued.state, ChangeState::Open as i32);
    assert!(queued.merge_queue_entry.is_some(), "auto-merge enqueued");
    assert_eq!(w.tip().await, tip, "nothing landed directly");

    // Both land through the queue.
    w.changes
        .enqueue(&w.cx("alice"), &w.repo, &cr1.id, LandMethod::Rebase, 0)
        .await
        .unwrap();
    let mut landed = Vec::new();
    while let Some((cr, _)) = w
        .changes
        .process_queue(&w.cx("system"), &w.repo, "main")
        .await
        .unwrap()
    {
        landed.push(cr.number);
        w.forge.flush().await.unwrap();
    }
    landed.sort_unstable();
    assert_eq!(landed, vec![cr1.number, cr2.number]);
    let tip2 = w.tip().await;
    assert!(w.file(&tip2, "src/one.rs").await.is_some());
    assert!(w.file(&tip2, "docs/two.md").await.is_some());
}

/// AS-CR-03: a five-CR stack goes through review and the merge queue and lands bottom first.
#[tokio::test]
async fn as_cr_03_five_cr_stack_through_review_and_merge_queue() {
    let w = world().await;
    w.policy.set(Some(POLICY));
    w.forge
        .orgs
        .update_repo_settings(&w.cx("owner"), &w.repo.id, 0, |s| {
            s.merge_queue_required = true;
            Ok(())
        })
        .await
        .unwrap();
    let mut parent = w.tip().await;
    let mut crs: Vec<ChangeRequest> = Vec::new();
    for i in 1..=5 {
        let head = w
            .commit(
                Some(&parent),
                &[(&format!("src/m{i}.rs"), Some(&format!("fn m{i}() {{}}\n")))],
                &format!("Part {i}"),
                "agent1",
            )
            .await;
        let cr = w
            .changes
            .open(
                &w.cx("agent1"),
                &w.repo,
                NewChange {
                    head: Some(head.clone()),
                    title: format!("Part {i}"),
                    depends_on: crs.last().map(|c| c.id.clone()).unwrap_or_default(),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            cr.change_ids.len(),
            1,
            "each CR covers only its own changes"
        );
        crs.push(cr);
        parent = head;
    }
    let stack = w
        .changes
        .stack(&w.cx("bob"), &w.repo, &crs[2].id)
        .await
        .unwrap();
    assert_eq!(
        stack.iter().map(|c| c.number).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    // Children cannot jump their parents.
    assert_eq!(
        w.changes
            .enqueue(&w.cx("agent1"), &w.repo, &crs[1].id, LandMethod::Rebase, 0)
            .await
            .unwrap_err()
            .subcode,
        "parent_not_queued"
    );
    // Direct lands are refused when the queue is required.
    assert_eq!(
        w.changes
            .land(
                &w.cx("agent1"),
                &w.repo,
                &crs[0].id,
                LandMethod::Rebase,
                "",
                0
            )
            .await
            .unwrap_err()
            .subcode,
        "merge_queue_required"
    );
    // Review: agent2 approves every CR; CI passes every head.
    for cr in &crs {
        w.approve("agent2", cr).await;
        w.check(
            cr.revisions[0].head.as_ref().unwrap(),
            "test",
            CheckStatus::Passed,
        )
        .await;
    }
    for cr in &crs {
        let e = w
            .changes
            .enqueue(&w.cx("agent1"), &w.repo, &cr.id, LandMethod::Rebase, 0)
            .await
            .unwrap();
        assert_eq!(e.state, MergeQueueState::Queued as i32);
    }
    let (queue, _) = w
        .changes
        .queue(&w.cx("bob"), &w.repo, "main")
        .await
        .unwrap();
    assert_eq!(
        queue.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        queue.iter().map(|e| e.change_number).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    // Main moves meanwhile; the queue still lands every CR, rebased, bottom first.
    w.advance_main(&[("docs/guide.md", Some("meanwhile\n"))], "Meanwhile")
        .await;
    let mut order = Vec::new();
    while let Some((cr, resp)) = w
        .changes
        .process_queue(&w.cx("system"), &w.repo, "main")
        .await
        .unwrap()
    {
        assert!(resp.landed.iter().filter(|l| !l.skipped).count() == 1);
        order.push(cr.number);
        w.forge.flush().await.unwrap();
    }
    assert_eq!(order, vec![1, 2, 3, 4, 5]);
    let tip = w.tip().await;
    for i in 1..=5 {
        assert!(w.file(&tip, &format!("src/m{i}.rs")).await.is_some());
    }
    assert_eq!(w.file(&tip, "docs/guide.md").await.unwrap(), "meanwhile\n");
    for cr in &crs {
        let c = w.get(&cr.id).await;
        assert_eq!(c.state, ChangeState::Landed as i32);
        assert!(c.merge_queue_entry.is_none());
        assert!(
            c.depends_on.is_empty(),
            "children were retargeted onto main"
        );
    }
    let (queue, per_minute) = w
        .changes
        .queue(&w.cx("bob"), &w.repo, "main")
        .await
        .unwrap();
    assert!(queue.is_empty());
    assert!(per_minute >= 1);
    let kinds = w.kinds().await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "merge_queue.entered").count(),
        5
    );
    assert_eq!(kinds.iter().filter(|k| *k == "cr.landed").count(), 5);
}

#[tokio::test]
async fn merge_queue_ejects_and_dequeues() {
    let w = world().await;
    let tip = w.tip().await;
    let h1 = w
        .commit(
            Some(&tip),
            &[("README.md", Some("# app\n\none\n"))],
            "One",
            "alice",
        )
        .await;
    let h2 = w
        .commit(
            Some(&tip),
            &[("README.md", Some("# app\n\ntwo\n"))],
            "Two",
            "bob",
        )
        .await;
    let h3 = w
        .commit(Some(&tip), &[("docs/x.md", Some("x\n"))], "Three", "carol")
        .await;
    let a = w.open("alice", &h1, "One").await;
    let b = w.open("bob", &h2, "Two").await;
    let c = w.open("carol", &h3, "Three").await;
    w.changes
        .enqueue(&w.cx("alice"), &w.repo, &a.id, LandMethod::Rebase, 0)
        .await
        .unwrap();
    w.changes
        .enqueue(&w.cx("bob"), &w.repo, &b.id, LandMethod::Rebase, 0)
        .await
        .unwrap();
    // Priority jumps ahead.
    let e = w
        .changes
        .enqueue(&w.cx("carol"), &w.repo, &c.id, LandMethod::Rebase, 5)
        .await
        .unwrap();
    assert_eq!(e.position, 1);
    w.changes
        .set_queue_state(
            &w.cx("system"),
            &w.repo,
            &c.id,
            MergeQueueState::Checking,
            Some(h3.clone()),
            "",
        )
        .await
        .unwrap();
    assert_eq!(
        w.get(&c.id).await.merge_queue_entry.unwrap().state,
        MergeQueueState::Checking as i32
    );
    w.changes
        .dequeue(&w.cx("carol"), &w.repo, &c.id)
        .await
        .unwrap();
    assert_eq!(
        w.changes
            .dequeue(&w.cx("carol"), &w.repo, &c.id)
            .await
            .unwrap_err()
            .subcode,
        "not_queued"
    );
    // a lands; b conflicts and is ejected.
    let (first, _) = w
        .changes
        .process_queue(&w.cx("system"), &w.repo, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, a.id);
    let e = w
        .changes
        .process_queue(&w.cx("system"), &w.repo, "main")
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::MergeConflict);
    let b2 = w.get(&b.id).await;
    assert!(b2.merge_queue_entry.is_none());
    assert_eq!(b2.state, ChangeState::Open as i32);
    assert!(
        w.changes
            .process_queue(&w.cx("system"), &w.repo, "main")
            .await
            .unwrap()
            .is_none()
    );
    // A new revision drops a queued CR from the queue.
    w.changes
        .enqueue(&w.cx("carol"), &w.repo, &c.id, LandMethod::Rebase, 0)
        .await
        .unwrap();
    let h4 = w
        .commit(Some(&h3), &[("docs/x.md", Some("xx\n"))], "More", "carol")
        .await;
    let c2 = w
        .changes
        .revise(&w.cx("carol"), &w.repo, &c.id, &h4)
        .await
        .unwrap();
    assert!(c2.merge_queue_entry.is_none());
}

// ----- service ----------------------------------------------------------------------------------------

mod service {
    use super::*;
    use bee_forge::changes::service::ChangeServices;
    use bee_proto::v1::change_service_server::ChangeService;
    use tokio_stream::StreamExt;

    fn rref(w: &World) -> Option<RepoRef> {
        Some(RepoRef {
            repo: Some(repo_ref::Repo::FullName(w.repo.full_name.clone())),
        })
    }

    fn reason<T: std::fmt::Debug>(
        r: std::result::Result<tonic::Response<T>, tonic::Status>,
    ) -> String {
        common::reason(r)
    }

    async fn setup() -> (World, ChangeServices, ObjectId) {
        let w = world().await;
        let s = ChangeServices::new(w.forge.clone(), w.changes.clone());
        let tip = w.tip().await;
        let h = w
            .commit(
                Some(&tip),
                &[("docs/guide.md", Some("svc\n"))],
                "Svc",
                "writer",
            )
            .await;
        (w, s, h)
    }

    #[tokio::test]
    async fn permission_matrix() {
        let (w, s, h) = setup().await;
        let writer = w.token("writer", bee_auth::Scope::new());
        let reader = w.token("reader", bee_auth::Scope::new());
        let outsider = w.token("outsider", bee_auth::Scope::new());
        let read_only = w.token("writer", common::actions(&[bee_policy::Action::Read]));
        let open = |tok: &str| {
            common::req(
                tok,
                OpenChangeRequest {
                    repo: rref(&w),
                    head: Some(h.clone()),
                    title: "Svc".into(),
                    ..Default::default()
                },
            )
        };
        // Members with the org's base role (read) cannot open; outsiders cannot see the
        // repo; a token scoped to read is refused by policy.
        assert_eq!(
            reason(s.open_change(open(&reader)).await),
            "PERMISSION_DENIED"
        );
        assert_eq!(reason(s.open_change(open(&outsider)).await), "NOT_FOUND");
        assert_eq!(
            reason(s.open_change(open(&read_only)).await),
            "POLICY_DENIED"
        );
        assert_eq!(
            reason(
                s.open_change(tonic::Request::new(OpenChangeRequest::default()))
                    .await
            ),
            "UNAUTHENTICATED"
        );
        let cr = s.open_change(open(&writer)).await.unwrap().into_inner();
        // Reads work for the reader.
        let got = s
            .get_change(common::req(
                &reader,
                GetChangeRequest {
                    repo: rref(&w),
                    id: cr.id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(got.number, cr.number);
        let listed = s
            .list_changes(common::req(
                &reader,
                ListChangesRequest {
                    repo: rref(&w),
                    query: "is:open".into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.changes.len(), 1);
        // Readers can comment in the conversation but not review or triage.
        s.create_change_comment(common::req(
            &reader,
            CreateChangeCommentRequest {
                repo: rref(&w),
                change_request_id: cr.id.clone(),
                body: "question".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
        assert_eq!(
            reason(
                s.submit_review(common::req(
                    &reader,
                    SubmitReviewRequest {
                        repo: rref(&w),
                        change_request_id: cr.id.clone(),
                        verdict: Verdict::Approve as i32,
                        ..Default::default()
                    },
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        assert_eq!(
            reason(
                s.close_change(common::req(
                    &reader,
                    CloseChangeRequest {
                        repo: rref(&w),
                        id: cr.id.clone(),
                        reason: "no".into()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        assert_eq!(
            reason(
                s.land_change(common::req(
                    &reader,
                    LandChangeRequest {
                        repo: rref(&w),
                        id: cr.id.clone(),
                        ..Default::default()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        // Dismissing a review needs maintain.
        let bob = w.token("bob", bee_auth::Scope::new());
        let rv = s
            .submit_review(common::req(
                &bob,
                SubmitReviewRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                    verdict: Verdict::Approve as i32,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            reason(
                s.dismiss_review(common::req(
                    &writer,
                    DismissReviewRequest {
                        repo: rref(&w),
                        change_request_id: cr.id.clone(),
                        review_id: rv.id.clone(),
                        reason: "x".into()
                    },
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        let owner = w.token("owner", bee_auth::Scope::new());
        s.dismiss_review(common::req(
            &owner,
            DismissReviewRequest {
                repo: rref(&w),
                change_request_id: cr.id.clone(),
                review_id: rv.id,
                reason: "x".into(),
            },
        ))
        .await
        .unwrap();
        // Queue priority needs maintain.
        assert_eq!(
            reason(
                s.enqueue_change(common::req(
                    &writer,
                    EnqueueChangeRequest {
                        repo: rref(&w),
                        id: cr.id.clone(),
                        priority: 3,
                        ..Default::default()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        // Denials of state-changing calls are audited.
        let scope = bee_forge::audit::org_scope(&w.org.id);
        let (entries, _) = w
            .forge
            .audit
            .query(&w.cx("owner"), &scope, &Default::default(), false, "", 500)
            .await
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.action == "cr.open" && e.result == "PERMISSION_DENIED")
        );
        // Archived repos refuse writes.
        w.forge
            .orgs
            .archive_repo(&w.cx("owner"), &w.repo.id)
            .await
            .unwrap();
        assert_eq!(
            reason(
                s.close_change(common::req(
                    &writer,
                    CloseChangeRequest {
                        repo: rref(&w),
                        id: cr.id.clone(),
                        reason: "x".into()
                    }
                ))
                .await
            ),
            "ARCHIVED"
        );
    }

    #[tokio::test]
    async fn lifecycle_over_rpc() {
        let (w, s, h) = setup().await;
        let writer = w.token("writer", bee_auth::Scope::new());
        let bob = w.token("bob", bee_auth::Scope::new());
        let reader = w.token("reader", bee_auth::Scope::new());
        let cr = s
            .open_change(common::req(
                &writer,
                OpenChangeRequest {
                    repo: rref(&w),
                    head: Some(h.clone()),
                    title: "Svc".into(),
                    labels: vec!["docs".into()],
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let by_number = s
            .get_change_by_number(common::req(
                &reader,
                GetChangeByNumberRequest {
                    repo: rref(&w),
                    number: cr.number,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(by_number.id, cr.id);
        let c = s
            .create_change_comment(common::req(
                &bob,
                CreateChangeCommentRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                    body: "line note".into(),
                    path: b"docs/guide.md".to_vec(),
                    side: "new".into(),
                    start_line: 1,
                    end_line: 1,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let threads = s
            .list_review_threads(common::req(
                &reader,
                ListReviewThreadsRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .threads;
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].comments[0].id, c.id);
        s.reply_to_review_thread(common::req(
            &writer,
            ReplyToReviewThreadRequest {
                repo: rref(&w),
                thread_id: threads[0].id.clone(),
                body: "done".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
        let t = s
            .resolve_review_thread(common::req(
                &writer,
                ResolveReviewThreadRequest {
                    repo: rref(&w),
                    thread_id: threads[0].id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(t.resolved && t.comments.len() == 2);
        let t = s
            .unresolve_review_thread(common::req(
                &bob,
                UnresolveReviewThreadRequest {
                    repo: rref(&w),
                    thread_id: threads[0].id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(!t.resolved);
        s.set_file_viewed(common::req(
            &bob,
            SetFileViewedRequest {
                repo: rref(&w),
                change_request_id: cr.id.clone(),
                path: b"docs/guide.md".to_vec(),
                viewed: true,
            },
        ))
        .await
        .unwrap();
        let viewed = s
            .list_viewed_files(common::req(
                &bob,
                ListViewedFilesRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(viewed.files.len(), 1);
        let diff: Vec<DiffItem> = s
            .diff_change_revisions(common::req(
                &reader,
                DiffChangeRevisionsRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                    include_hunks: true,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .map(|r| r.unwrap())
            .collect()
            .await;
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].diff_class, "docs");
        s.submit_review(common::req(
            &bob,
            SubmitReviewRequest {
                repo: rref(&w),
                change_request_id: cr.id.clone(),
                verdict: Verdict::Approve as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
        let reviews = s
            .list_reviews(common::req(
                &reader,
                ListReviewsRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reviews.reviews.len(), 1);
        let ev = s
            .evaluate_landing(common::req(
                &writer,
                EvaluateLandingRequest {
                    repo: rref(&w),
                    change_request_id: cr.id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(ev.allowed && ev.mergeable && ev.unresolved_thread_ids.len() == 1);
        let templates = s
            .list_change_templates(common::req(
                &reader,
                ListChangeTemplatesRequest {
                    repo: rref(&w),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(templates.templates.len(), 2);
        let stack = s
            .list_change_stack(common::req(
                &reader,
                ListChangeStackRequest {
                    repo: rref(&w),
                    id: cr.id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(stack.changes.len(), 1);
        let ctx = s
            .get_context(common::req(
                &reader,
                GetContextRequest {
                    repo: rref(&w),
                    paths: vec![b"docs".to_vec()],
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ctx.open_changes_touching.len(), 1);
        let landed = s
            .land_change(common::req(
                &writer,
                LandChangeRequest {
                    repo: rref(&w),
                    id: cr.id.clone(),
                    method: LandMethod::Rebase as i32,
                    expected_revision: 1,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(landed.change.unwrap().state, ChangeState::Landed as i32);
        let reverted = s
            .revert_change(common::req(
                &writer,
                RevertChangeRequest {
                    repo: rref(&w),
                    id: cr.id.clone(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reverted.revert_of, cr.id);
        let q = s
            .get_merge_queue(common::req(
                &reader,
                GetMergeQueueRequest {
                    repo: rref(&w),
                    r#ref: "main".into(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(q.entries.is_empty());
        let conflicts = s
            .list_land_conflicts(common::req(
                &reader,
                ListLandConflictsRequest {
                    repo: rref(&w),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(conflicts.conflicts.is_empty());
        let cs = s
            .create_changeset_comment(common::req(
                &reader,
                CreateChangesetCommentRequest {
                    repo: rref(&w),
                    changeset: Some(h.clone()),
                    body: "nice commit".into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let listed = s
            .list_changeset_comments(common::req(
                &reader,
                ListChangesetCommentsRequest {
                    repo: rref(&w),
                    changeset: Some(h.clone()),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.comments[0].id, cs.id);
        // Stale expected revision.
        assert_eq!(
            reason(
                s.land_change(common::req(
                    &writer,
                    LandChangeRequest {
                        repo: rref(&w),
                        id: reverted.id.clone(),
                        expected_revision: 9,
                        ..Default::default()
                    }
                ))
                .await
            ),
            "VERSION_MISMATCH"
        );
        // Missing fields name the field.
        assert_eq!(
            reason(
                s.revise_change(common::req(
                    &writer,
                    ReviseChangeRequest {
                        repo: rref(&w),
                        id: reverted.id.clone(),
                        head: None
                    }
                ))
                .await
            ),
            "INVALID_ARGUMENT"
        );
    }
}
