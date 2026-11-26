//! Forks end to end over real history (`HistoryVcs` and `HistoryForkVcs` on in-memory VCS
//! stores), change requests, CI with the local runner and enterprise moderation:
//!
//! - an outside contributor forks a public repo (no object written: the fork shares the
//!   upstream's namespace), commits in the fork, opens a change request upstream, its CI run
//!   waits for approval as a first-time contributor and gets no secrets, a maintainer
//!   approves, checks pass and the change lands upstream;
//! - content only in the fork is not readable through the upstream until a change request
//!   publishes it, and redaction in the shared namespace reaches every member;
//! - sync (fast-forward, rebase, conflict), the network listing, pushes to the fork ref
//!   revising the change request, maintainers' edits written back to the fork;
//! - private forks (who may fork, visibility, the upstream maintainers' read access), and
//!   deleting the upstream or a fork.

use crate::common;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bee_auth::KeyRing;
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::{ChangeVcs, NewCommit};
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams};
use bee_forge::ci::runner::{InProcessApi, LocalRunner, RunnerConfig};
use bee_forge::ci::runners::RunnerOwner;
use bee_forge::ci::secrets::VaultScope;
use bee_forge::ci::{Ci, CiConfig, CiHooks, CiServices, HistoryCiVcs, HmacKey, MemoryCiBlobs};
use bee_forge::enterprise::moderation::{InMemoryStanding, Moderation, ModerationConfig};
use bee_forge::error::{ForgeError, Result};
use bee_forge::forge::Hooks;
use bee_forge::forks::{Forks, HistoryForkVcs, ModerationGate};
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PolicySource, PrincipalDirectory};
use bee_forge::ids::{Clock, SystemClock, hex};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::service::ForgeServices;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::change_service_server::ChangeService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::workflow_service_server::WorkflowService;
use bee_proto::v1::*;
use bee_store::{
    AccessCounters, MemoryBlobStore, MemoryMetaStore, MetaStore, ReadFrom, RepoId, Writer,
};

// ----- fixture -------------------------------------------------------------------------------

const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("maintainer", PrincipalKind::Human),
    ("member", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("stranger", PrincipalKind::Human),
];

const POLICY: &str = r#"version = 1

[members]
admin = ["owner"]
maintain = ["maintainer"]
read = ["@everyone"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@everyone"]

[[land]]
ref = "main"
required_checks = ["test"]
"#;

/// The upstream's policy (the `test` check is required on `main`); forks have none, so
/// built-in roles decide there.
#[derive(Default)]
struct TestPolicy(RwLock<String>);

#[async_trait]
impl PolicySource for TestPolicy {
    async fn policy(&self, repo: &Repo) -> Result<Option<bee_policy::Policy>> {
        if *self.0.read().unwrap() != repo.id {
            return Ok(None);
        }
        bee_policy::Policy::parse(POLICY)
            .map(Some)
            .map_err(|e| ForgeError::invalid("invalid_policy", e.to_string()))
    }
}

const WORKFLOW: &str = "on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"secret=[${{ secrets.TOKEN }}]\"\n";

struct World {
    forge: Arc<Forge>,
    changes: Arc<Changes>,
    ci: Arc<Ci>,
    forks: Arc<Forks>,
    standing: Arc<InMemoryStanding>,
    repos: ForgeServices,
    crs: ChangeServices,
    wf: CiServices,
    vcs: Arc<HistoryVcs>,
    meta: Arc<MemoryMetaStore>,
    counters: Arc<AccessCounters>,
    ring: KeyRing,
    clock: Arc<dyn Clock>,
    org: Org,
    upstream: Repo,
    work: tempfile::TempDir,
}

fn rref(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(repo.id.clone())),
    })
}

async fn world(visibility: Visibility) -> World {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let counters = Arc::new(AccessCounters::new());
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::with_counters(
        clock.clone(),
        counters.clone(),
    ));
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
    let ring = KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let policy = Arc::new(TestPolicy::default());
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta.clone(), clock.clone())),
        policy: Some(policy.clone()),
        ..Hooks::default()
    };
    let (forge, _) = Forge::open(
        Arc::new(MemoryStore::new()),
        clock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        hooks,
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer.clone(), clock.clone()));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    changes.attach(&forge);
    forge.set_changes(changes.clone());
    let standing = Arc::new(InMemoryStanding::new());
    let moderation = Moderation::new(forge.clone(), ModerationConfig::new(standing.clone()));
    forge.set_write_guard(moderation.clone());
    let ci = Ci::new(
        &forge,
        CiHooks {
            vcs: Arc::new(HistoryCiVcs::new(vcs.clone())),
            blobs: Arc::new(MemoryCiBlobs::new()),
            keys: Arc::new(HmacKey::new("k1", b"oidc-key")),
            secret_key: [3u8; 32],
            url_key: b"url-key".to_vec(),
            signer: Arc::new(KeyRing::new(
                bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap(),
            )),
            config: CiConfig {
                keepalive_ms: 50,
                ..CiConfig::default()
            },
        },
    );
    ci.attach(&forge);
    ci.set_fork_gate(Arc::new(ModerationGate(moderation)));
    let forks = Forks::new(
        &forge,
        Arc::new(HistoryForkVcs::new(writer, clock.clone())),
        vcs.clone(),
    );
    forks.attach(&forge);

    let cx = Cx::new("owner", PrincipalKind::Human);
    let name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
    let org = forge
        .orgs
        .create_org(&cx, &name, "", directory.get("owner").await.unwrap())
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    let upstream = forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            NewRepo {
                id: forge.db.ids().ulid().unwrap(),
                name: "app".into(),
                visibility,
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    forge
        .hooks
        .provisioner
        .create_repo(&upstream, &bee_forge::hooks::RepoInit::default())
        .await
        .unwrap();
    *policy.0.write().unwrap() = upstream.id.clone();
    for who in ["member", "maintainer"] {
        forge
            .orgs
            .set_member(
                &cx,
                &org.id,
                directory.get(who).await.unwrap(),
                Role::Member,
            )
            .await
            .unwrap();
    }
    forge
        .orgs
        .add_collaborator(
            &cx,
            &upstream.id,
            directory.get("maintainer").await.unwrap(),
            RepoRole::Maintain,
            "",
        )
        .await
        .unwrap();
    // Personal namespaces: orgs named after the logins.
    for who in ["outsider", "member", "stranger"] {
        let pcx = Cx::new(who, PrincipalKind::Human);
        let o = forge
            .orgs
            .create_org(
                &pcx,
                &format!("{who}-login"),
                "",
                directory.get(who).await.unwrap(),
            )
            .await
            .unwrap();
        forge.hooks.provisioner.create_org(&o).await.unwrap();
    }
    World {
        repos: ForgeServices::new(forge.clone()),
        crs: ChangeServices::new(forge.clone(), changes.clone()),
        wf: CiServices::new(forge.clone(), ci.clone()),
        forge,
        changes,
        ci,
        forks,
        standing,
        vcs,
        meta,
        counters,
        ring,
        clock,
        org,
        upstream,
        work: tempfile::tempdir().unwrap(),
    }
}

impl World {
    fn cx(&self, who: &str) -> Cx {
        Cx::new(who, PrincipalKind::Human)
    }

    fn token(&self, who: &str) -> String {
        self.ring
            .mint(
                &bee_auth::MintRequest {
                    principal: bee_auth::Principal::human(who, who),
                    on_behalf_of: vec![],
                    scope: bee_auth::Scope::new(),
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    async fn tip(&self, repo: &Repo, name: &str) -> Option<ObjectId> {
        self.vcs
            .ref_tip(repo, name)
            .await
            .unwrap()
            .and_then(|r| r.target)
    }

    /// Commits `edits` on `branch` of `repo` (starting from `main` when the branch is new)
    /// and moves the branch.
    async fn commit(
        &self,
        repo: &Repo,
        branch: &str,
        edits: &[(&str, Option<&str>)],
        who: &str,
    ) -> ObjectId {
        let parent = match self.tip(repo, branch).await {
            Some(t) => Some(t),
            None => self.tip(repo, "main").await,
        };
        let c = self
            .vcs
            .create_commit(
                repo,
                NewCommit {
                    parent,
                    edits: edits
                        .iter()
                        .map(|(p, c)| (p.to_string(), c.map(|s| s.as_bytes().to_vec())))
                        .collect(),
                    message: format!("{who} edits {}", edits[0].0),
                    author: who.into(),
                    creator: who.into(),
                    chain: vec![who.into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        self.vcs.set_ref(repo, branch, &c).await.unwrap();
        c
    }

    /// Moves `branch` of `repo` to a new commit and appends the `ref.updated` event the VCS
    /// bridge would, then lets every listener run.
    async fn push(
        &self,
        repo: &Repo,
        branch: &str,
        edits: &[(&str, Option<&str>)],
        who: &str,
    ) -> ObjectId {
        let c = self.commit(repo, branch, edits, who).await;
        let version = self
            .vcs
            .ref_tip(repo, branch)
            .await
            .unwrap()
            .unwrap()
            .version;
        self.forge
            .events
            .append(
                &Cx::system(),
                Event {
                    kind: "ref.updated".into(),
                    repo_id: repo.id.clone(),
                    org_id: repo.org_id.clone(),
                    principal: who.into(),
                    body: Some(event::Body::RefUpdated(Ref {
                        name: branch.into(),
                        target: Some(c.clone()),
                        version,
                    })),
                    ..Default::default()
                },
                &format!("test:{}:{}", branch, hex(&c.raw)),
            )
            .await
            .unwrap();
        self.forge.flush().await.unwrap();
        c
    }

    async fn fork_as(&self, who: &str, upstream: &Repo) -> Repo {
        self.repos
            .fork_repo(common::req(
                &self.token(who),
                ForkRepoRequest {
                    repo: rref(upstream),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn open_from_fork(
        &self,
        who: &str,
        fork: &Repo,
        head: &ObjectId,
        head_ref: &str,
        maintainer_edits: bool,
    ) -> ChangeRequest {
        let cr = self
            .crs
            .open_change(common::req(
                &self.token(who),
                OpenChangeRequest {
                    repo: rref(&self.upstream),
                    target_ref: "main".into(),
                    head: Some(head.clone()),
                    title: format!("{who}'s change"),
                    head_repo: rref(fork),
                    head_ref: head_ref.into(),
                    allow_maintainer_edits: maintainer_edits,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        self.forge.flush().await.unwrap();
        cr
    }

    async fn runs(&self) -> Vec<WorkflowRun> {
        self.ci
            .list_runs(&self.cx("owner"), &self.upstream.id, "")
            .await
            .unwrap()
    }

    async fn runner(&self) -> LocalRunner {
        let cx = self.cx("owner");
        let owner = RunnerOwner::Repo(self.upstream.id.clone(), self.org.id.clone());
        let (token, _) = self.ci.runner_token(&cx, &owner, "register").await.unwrap();
        let (runner, cred) = self
            .ci
            .register_runner(
                &Cx::system(),
                &token,
                "r",
                RunnerOs::Linux,
                RunnerArch::X64,
                &["ubuntu-latest".to_string()],
                "",
                false,
                "test",
            )
            .await
            .unwrap();
        LocalRunner::new(
            Arc::new(InProcessApi::new(self.wf.clone(), runner.id, cred, vec![])),
            RunnerConfig {
                work_root: self.work.path().to_path_buf(),
                name: "r".into(),
                heartbeat: Duration::from_millis(200),
                ..RunnerConfig::default()
            },
        )
    }

    async fn log(&self, run: &str) -> String {
        let jobs = self
            .ci
            .list_jobs(&self.cx("owner"), &self.upstream.id, run, 0, false)
            .await
            .unwrap();
        let mut out = String::new();
        for j in jobs {
            out.push_str(&String::from_utf8_lossy(
                &self
                    .ci
                    .job_log_bytes(&self.cx("owner"), &self.upstream.id, &j.id)
                    .await
                    .unwrap(),
            ));
        }
        out
    }

    async fn object_count(&self) -> usize {
        let org = bee_store::OrgId(bee_core::Ulid::parse(&self.org.id).unwrap());
        self.meta
            .list_objects(org, None, 100_000)
            .await
            .unwrap()
            .len()
    }

    async fn paths(&self, repo: &Repo, object: &ObjectId) -> usize {
        let id = bee_core::ObjectId::from_bytes(&object.raw).unwrap();
        self.meta
            .object_paths(
                RepoId(bee_core::Ulid::parse(&repo.id).unwrap()),
                &id,
                ReadFrom::Primary,
            )
            .await
            .unwrap()
            .len()
    }
}

fn reason<T: std::fmt::Debug>(r: std::result::Result<tonic::Response<T>, tonic::Status>) -> String {
    common::reason(r)
}

// ----- the outside contributor flow -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outside_contributor_forks_opens_cr_waits_for_ci_approval_and_lands() {
    let w = world(Visibility::Public).await;
    w.commit(
        &w.upstream,
        "main",
        &[
            ("README.md", Some("# app\n")),
            ("src/lib.rs", Some("pub fn f() {}\n")),
            (".github/workflows/ci.yml", Some(WORKFLOW)),
        ],
        "owner",
    )
    .await;
    w.ci.put_secret(
        &w.cx("owner"),
        &VaultScope::Repo(w.upstream.id.clone(), w.org.id.clone()),
        "TOKEN",
        b"s3cr3t-value",
        SecretVisibility::Unspecified,
        vec![],
    )
    .await
    .unwrap();

    // Fork: no object row and no blob written; the fork shares the upstream's namespace.
    let objects = w.object_count().await;
    let puts = w.counters.snapshot().blob.puts;
    let fork = w.fork_as("outsider", &w.upstream).await;
    assert_eq!(fork.fork_parent_id, w.upstream.id);
    assert_eq!(fork.full_name, "outsider-login/app");
    assert_eq!(fork.visibility, Visibility::Public as i32);
    assert_eq!(
        w.object_count().await,
        objects,
        "no object rows for the fork"
    );
    assert_eq!(
        w.counters.snapshot().blob.puts,
        puts,
        "no blob bytes for the fork"
    );
    let up_main = w.tip(&w.upstream, "main").await.unwrap();
    assert_eq!(w.tip(&fork, "main").await.unwrap(), up_main);
    let row = w
        .meta
        .get_repo(RepoId(bee_core::Ulid::parse(&fork.id).unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.org.0.to_text(),
        w.org.id,
        "the fork lives in the upstream namespace"
    );
    // Forking again into the same namespace returns the existing fork.
    assert_eq!(w.fork_as("outsider", &w.upstream).await.id, fork.id);

    // A change in the fork is not readable through the upstream.
    let head = w
        .commit(
            &fork,
            "feature",
            &[("src/new.rs", Some("pub fn g() {}\n"))],
            "outsider",
        )
        .await;
    let blob = w
        .vcs
        .file_id(&fork, &head, "src/new.rs")
        .await
        .unwrap()
        .unwrap();
    assert!(w.paths(&fork, &blob).await > 0);
    assert_eq!(
        w.paths(&w.upstream, &blob).await,
        0,
        "fork-only content stays in the fork"
    );

    // The outsider (read access upstream) opens a change request from the fork.
    let cr = w
        .open_from_fork("outsider", &fork, &head, "feature", true)
        .await;
    assert_eq!(cr.head_repo_id, fork.id);
    assert_eq!(cr.head_ref, "feature");
    assert!(cr.allow_maintainer_edits);
    assert!(
        w.paths(&w.upstream, &blob).await > 0,
        "the change request publishes its stack to the upstream"
    );
    // A stranger without write access to the fork cannot open one from it.
    let e = w
        .crs
        .open_change(common::req(
            &w.token("stranger"),
            OpenChangeRequest {
                repo: rref(&w.upstream),
                target_ref: "main".into(),
                head: Some(head.clone()),
                title: "not mine".into(),
                head_repo: rref(&fork),
                head_ref: "feature".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(e), "PERMISSION_DENIED");

    // CI: the first-time contributor's run waits for approval.
    let runs = w.runs().await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run.event, "pull_request");
    assert_eq!(run.status, WorkflowStatus::ActionRequired as i32);
    assert!(run.approval_required);
    let runner = w.runner().await;
    assert!(runner.run_until_idle(100).await.unwrap().is_empty());
    // Landing is refused while the required check has not run.
    assert!(
        w.changes
            .land(
                &w.cx("maintainer"),
                &w.upstream,
                &cr.id,
                LandMethod::Rebase,
                "",
                0
            )
            .await
            .is_err()
    );
    // The contributor cannot approve their own run; a maintainer can.
    let e =
        w.wf.approve_workflow_run(common::req(
            &w.token("outsider"),
            ApproveWorkflowRunRequest {
                repo: rref(&w.upstream),
                run_id: run.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(e), "PERMISSION_DENIED");
    w.wf.approve_workflow_run(common::req(
        &w.token("maintainer"),
        ApproveWorkflowRunRequest {
            repo: rref(&w.upstream),
            run_id: run.id.clone(),
        },
    ))
    .await
    .unwrap();
    runner.run_until_idle(100).await.unwrap();
    let done =
        w.ci.get_run(&w.cx("owner"), &w.upstream.id, &run.id)
            .await
            .unwrap();
    assert_eq!(done.conclusion, WorkflowConclusion::Success as i32);
    let log = w.log(&run.id).await;
    assert!(log.contains("secret=[]"), "fork runs get no secrets: {log}");
    assert!(!log.contains("s3cr3t-value"));

    // Checks pass: the maintainer lands it upstream through the lander.
    let (landed, _) = w
        .changes
        .land(
            &w.cx("maintainer"),
            &w.upstream,
            &cr.id,
            LandMethod::Rebase,
            "",
            0,
        )
        .await
        .unwrap();
    assert_eq!(landed.state, ChangeState::Landed as i32);
    let main = w.tip(&w.upstream, "main").await.unwrap();
    assert_eq!(
        w.vcs
            .read_file(&w.upstream, &main, "src/new.rs")
            .await
            .unwrap()
            .unwrap(),
        b"pub fn g() {}\n"
    );

    // After a landed contribution the contributor is no longer first-time.
    w.standing.add_contribution(&w.upstream.id, "outsider");
    let head2 = w
        .commit(&fork, "second", &[("docs.md", Some("more\n"))], "outsider")
        .await;
    let before = w.runs().await.len();
    w.open_from_fork("outsider", &fork, &head2, "second", false)
        .await;
    let runs = w.runs().await;
    assert_eq!(runs.len(), before + 1);
    let newest = runs.iter().find(|r| r.id != run.id).expect("second run");
    assert_eq!(
        newest.status,
        WorkflowStatus::Queued as i32,
        "returning contributors run at once: {newest:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction_in_the_shared_namespace_reaches_every_fork() {
    let w = world(Visibility::Public).await;
    w.commit(
        &w.upstream,
        "main",
        &[("leak.txt", Some("password=hunter2\n"))],
        "owner",
    )
    .await;
    let fork = w.fork_as("outsider", &w.upstream).await;
    let main = w.tip(&w.upstream, "main").await.unwrap();
    let blob = w
        .vcs
        .file_id(&w.upstream, &main, "leak.txt")
        .await
        .unwrap()
        .unwrap();
    let id = bee_core::ObjectId::from_bytes(&blob.raw).unwrap();
    let org = bee_store::OrgId(bee_core::Ulid::parse(&w.org.id).unwrap());
    w.meta
        .redact(bee_store::RedactionRow {
            org,
            object: id,
            principal: "owner".into(),
            reason: "leaked credential".into(),
            at_ms: w.clock.now_ms(),
        })
        .await
        .unwrap();
    let fork_repo = RepoId(bee_core::Ulid::parse(&fork.id).unwrap());
    assert!(
        w.meta.redaction_epoch(fork_repo).await.unwrap() > 0,
        "the fork's redaction epoch moves with the upstream's"
    );
    let e = w.vcs.read_file(&fork, &main, "leak.txt").await;
    assert!(e.is_err(), "redacted bytes are gone through the fork too");
}

// ----- sync, network, follow pushes, maintainer edits ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_fork_fast_forwards_rebases_and_reports_conflicts() {
    let w = world(Visibility::Public).await;
    w.commit(&w.upstream, "main", &[("a.txt", Some("1\n"))], "owner")
        .await;
    let fork = w.fork_as("outsider", &w.upstream).await;
    let sync = |who: &'static str| {
        let w = &w;
        let fork = fork.clone();
        async move {
            w.repos
                .sync_fork(common::req(
                    &w.token(who),
                    SyncForkRequest {
                        repo: rref(&fork),
                        r#ref: String::new(),
                    },
                ))
                .await
        }
    };
    // Up to date: nothing to do.
    let s = sync("outsider").await.unwrap().into_inner();
    assert_eq!(s.changesets_pulled, 0);

    // The upstream moves on: the fork fast-forwards.
    let up = w
        .commit(&w.upstream, "main", &[("b.txt", Some("2\n"))], "owner")
        .await;
    let s = sync("outsider").await.unwrap().into_inner();
    assert_eq!(s.changesets_pulled, 1);
    assert_eq!(s.r#ref.unwrap().target.unwrap(), up);
    assert_eq!(w.tip(&fork, "main").await.unwrap(), up);
    // Only a writer of the fork may sync it.
    assert_eq!(reason(sync("stranger").await), "PERMISSION_DENIED");

    // Both moved: the fork's own changeset is rebased onto the upstream tip.
    w.commit(&fork, "main", &[("mine.txt", Some("fork\n"))], "outsider")
        .await;
    let up2 = w
        .commit(&w.upstream, "main", &[("c.txt", Some("3\n"))], "owner")
        .await;
    let s = sync("outsider").await.unwrap().into_inner();
    let new = s.r#ref.unwrap().target.unwrap();
    assert!(w.vcs.is_ancestor(&fork, &up2, &new).await.unwrap());
    for f in ["a.txt", "b.txt", "c.txt", "mine.txt"] {
        assert!(
            w.vcs.read_file(&fork, &new, f).await.unwrap().is_some(),
            "{f} after the rebase"
        );
    }

    // Conflicting edits: MERGE_CONFLICT, the fork ref stays.
    w.commit(&fork, "main", &[("a.txt", Some("fork side\n"))], "outsider")
        .await;
    let before = w.tip(&fork, "main").await.unwrap();
    w.commit(
        &w.upstream,
        "main",
        &[("a.txt", Some("upstream side\n"))],
        "owner",
    )
    .await;
    assert_eq!(reason(sync("outsider").await), "MERGE_CONFLICT");
    assert_eq!(w.tip(&fork, "main").await.unwrap(), before);

    // Syncing a repo that is not a fork is refused.
    let e = w
        .repos
        .sync_fork(common::req(
            &w.token("owner"),
            SyncForkRequest {
                repo: rref(&w.upstream),
                r#ref: String::new(),
            },
        ))
        .await;
    assert_eq!(reason(e), "INVALID_ARGUMENT");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_listing_follows_pushes_and_maintainer_edits() {
    let w = world(Visibility::Public).await;
    w.commit(&w.upstream, "main", &[("a.txt", Some("1\n"))], "owner")
        .await;
    let f1 = w.fork_as("outsider", &w.upstream).await;
    let f2 = w.fork_as("member", &w.upstream).await;
    // A fork of a fork joins the same network.
    let f3 = w
        .repos
        .fork_repo(common::req(
            &w.token("stranger"),
            ForkRepoRequest {
                repo: rref(&f1),
                name: "app-copy".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let listed = w
        .repos
        .list_forks(common::req(
            &w.token("stranger"),
            ListForksRequest {
                repo: rref(&w.upstream),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut ids: Vec<String> = listed.forks.iter().map(|r| r.id.clone()).collect();
    ids.sort();
    let mut want = vec![f1.id.clone(), f2.id.clone()];
    want.sort();
    assert_eq!(ids, want, "direct forks only");
    let network = w.forks.network(&w.cx("owner"), &f3).await.unwrap();
    assert_eq!(network.len(), 4, "root and three forks");
    assert_eq!(network[0].id, w.upstream.id);
    assert_eq!(
        w.forks.root_of(&w.cx("owner"), &f3).await.unwrap(),
        w.upstream.id
    );

    // Pushes to the fork ref revise the change request.
    let head = w
        .commit(&f1, "feature", &[("x.txt", Some("x\n"))], "outsider")
        .await;
    let cr = w
        .open_from_fork("outsider", &f1, &head, "feature", false)
        .await;
    let pushed = w
        .push(&f1, "feature", &[("y.txt", Some("y\n"))], "outsider")
        .await;
    let now = w
        .changes
        .get(&w.cx("owner"), &w.upstream, &cr.id)
        .await
        .unwrap();
    assert_eq!(now.revisions.len(), 2);
    assert_eq!(now.revisions[1].head.clone().unwrap(), pushed);

    // A maintainer's edit is refused while the author does not allow it.
    let edit = w
        .commit(
            &w.upstream,
            "fixup",
            &[("x.txt", Some("x fixed\n"))],
            "maintainer",
        )
        .await;
    let e = w
        .crs
        .revise_change(common::req(
            &w.token("maintainer"),
            ReviseChangeRequest {
                repo: rref(&w.upstream),
                id: cr.id.clone(),
                head: Some(edit.clone()),
            },
        ))
        .await;
    assert_eq!(reason(e), "PERMISSION_DENIED");
    // Only the author may allow it.
    let e = w
        .crs
        .edit_change(common::req(
            &w.token("maintainer"),
            EditChangeRequest {
                repo: rref(&w.upstream),
                id: cr.id.clone(),
                allow_maintainer_edits: Some(true),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(e), "PERMISSION_DENIED");
    w.crs
        .edit_change(common::req(
            &w.token("outsider"),
            EditChangeRequest {
                repo: rref(&w.upstream),
                id: cr.id.clone(),
                allow_maintainer_edits: Some(true),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    // The fixup builds on the CR head so the edit is a new revision of the same stack.
    let fixup = w
        .vcs
        .create_commit(
            &w.upstream,
            NewCommit {
                parent: Some(pushed.clone()),
                edits: vec![("x.txt".into(), Some(b"x fixed\n".to_vec()))],
                message: "maintainer fixup".into(),
                author: "maintainer".into(),
                creator: "maintainer".into(),
                chain: vec!["maintainer".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    let revised = w
        .crs
        .revise_change(common::req(
            &w.token("maintainer"),
            ReviseChangeRequest {
                repo: rref(&w.upstream),
                id: cr.id.clone(),
                head: Some(fixup.clone()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        revised.revisions.last().unwrap().head.clone().unwrap(),
        fixup
    );
    assert_eq!(
        w.tip(&f1, "feature").await.unwrap(),
        fixup,
        "the edit is written back to the contributor's branch"
    );
}

// ----- private forks and deletion -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_forks_follow_the_upstream_rules() {
    let w = world(Visibility::Private).await;
    w.commit(&w.upstream, "main", &[("a.txt", Some("1\n"))], "owner")
        .await;
    let fork_req = |who: &str| {
        common::req(
            &w.token(who),
            ForkRepoRequest {
                repo: rref(&w.upstream),
                ..Default::default()
            },
        )
    };
    // Outsiders cannot see the private repo at all.
    assert_eq!(
        reason(w.repos.fork_repo(fork_req("outsider")).await),
        "NOT_FOUND"
    );
    // Members may fork private repos only when the org allows it.
    assert_eq!(
        reason(w.repos.fork_repo(fork_req("member")).await),
        "PERMISSION_DENIED"
    );
    w.forge
        .orgs
        .update_settings(&w.cx("owner"), &w.org.id, 0, |s| {
            s.members_can_fork_private_repos = true
        })
        .await
        .unwrap();
    let fork = w
        .repos
        .fork_repo(fork_req("member"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fork.visibility, Visibility::Private as i32);
    assert_eq!(fork.full_name, "member-login/app");

    let get =
        |who: &str, repo: &Repo| common::req(&w.token(who), GetRepoRequest { repo: rref(repo) });
    // The fork owner and the upstream's maintainers see it; others do not.
    w.repos.get_repo(get("member", &fork)).await.unwrap();
    w.repos.get_repo(get("maintainer", &fork)).await.unwrap();
    assert_eq!(
        reason(w.repos.get_repo(get("stranger", &fork)).await),
        "NOT_FOUND"
    );
    // The fork's visibility follows the upstream.
    let e = w
        .repos
        .update_repo(common::req(
            &w.token("member"),
            UpdateRepoRequest {
                repo: rref(&fork),
                visibility: Visibility::Public as i32,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(e), "INVALID_ARGUMENT");

    // Forking can be switched off per repo.
    w.forge
        .orgs
        .update_repo_settings(&w.cx("owner"), &w.upstream.id, 0, |s| {
            s.allow_forking = false;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        reason(w.repos.fork_repo(fork_req("owner")).await),
        "PERMISSION_DENIED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_the_upstream_keeps_forks_and_deleting_a_fork_leaves_the_network() {
    let w = world(Visibility::Public).await;
    w.commit(&w.upstream, "main", &[("a.txt", Some("1\n"))], "owner")
        .await;
    let fork = w.fork_as("outsider", &w.upstream).await;
    let other = w.fork_as("member", &w.upstream).await;
    w.repos
        .delete_repo(common::req(
            &w.token("owner"),
            DeleteRepoRequest {
                repo: rref(&w.upstream),
                confirm_full_name: w.upstream.full_name.clone(),
            },
        ))
        .await
        .unwrap();
    // The fork keeps working and keeps its history.
    w.repos
        .get_repo(common::req(
            &w.token("outsider"),
            GetRepoRequest { repo: rref(&fork) },
        ))
        .await
        .unwrap();
    let head = w
        .commit(&fork, "main", &[("b.txt", Some("2\n"))], "outsider")
        .await;
    assert!(
        w.vcs
            .read_file(&fork, &head, "a.txt")
            .await
            .unwrap()
            .is_some()
    );
    // Syncing needs the upstream.
    let sync = || {
        w.repos.sync_fork(common::req(
            &w.token("outsider"),
            SyncForkRequest {
                repo: rref(&fork),
                r#ref: String::new(),
            },
        ))
    };
    assert_eq!(reason(sync().await), "INVALID_STATE");
    // Restoring the upstream restores the network.
    w.repos
        .restore_repo(common::req(
            &w.token("owner"),
            RestoreRepoRequest {
                repo_id: w.upstream.id.clone(),
            },
        ))
        .await
        .unwrap();
    sync().await.unwrap();

    // A deleted fork leaves the listing.
    w.repos
        .delete_repo(common::req(
            &w.token("member"),
            DeleteRepoRequest {
                repo: rref(&other),
                confirm_full_name: other.full_name.clone(),
            },
        ))
        .await
        .unwrap();
    let listed = w
        .repos
        .list_forks(common::req(
            &w.token("owner"),
            ListForksRequest {
                repo: rref(&w.upstream),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let ids: BTreeMap<String, String> = listed
        .forks
        .iter()
        .map(|r| (r.id.clone(), r.full_name.clone()))
        .collect();
    assert_eq!(ids.len(), 1);
    assert!(ids.contains_key(&fork.id));
}
