//! Keeping untested combinations of landed changes out of deploys (SPEC §11.6, PRODUCT
//! §4.14) over real history, the changes domain, CI and the local runner:
//!
//! - `green/<ref>` moves forward only to tips whose post-land checks all passed;
//! - `require_green` holds or refuses workflow deploys, and `CreateDeployment` can deploy
//!   the newest green tip instead;
//! - a post-land failure is re-run at the failing tip before anyone is blamed (flaky
//!   failures revert nothing);
//! - a culprit in the middle of a multi-changeset CR reverts that CR;
//! - two CRs that pass alone but fail together: by default the combination is detected after
//!   landing, bisected and the right CR reverted while deploys stay on the last green tip;
//!   with `merge_queue_check_combined` the queue tests the combination and ejects the second
//!   CR before it lands.

use crate::common;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::bisect::{BisectConfig, Bisector, StartBisect};
use bee_forge::changes::vcs::{ChangeVcs, NewCommit};
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams, NewChange, QueueStep};
use bee_forge::checks::Report;
use bee_forge::ci::deployments::NewDeployment;
use bee_forge::ci::environments::EnvironmentSpec;
use bee_forge::ci::runner::{InProcessApi, LocalRunner, RunnerConfig};
use bee_forge::ci::runners::RunnerOwner;
use bee_forge::ci::{Ci, CiConfig, CiHooks, CiServices, HistoryCiVcs, HmacKey, MemoryCiBlobs};
use bee_forge::error::{ForgeError, Result};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PolicySource, PrincipalDirectory};
use bee_forge::ids::{Clock, SystemClock, hex};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

// ----- fixture -------------------------------------------------------------------------------

const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("agent", PrincipalKind::Agent),
];

/// `test` must pass on `main` (the opt-in queue scenario).
const REQUIRE_TEST: &str = r#"version = 1

[members]
write = ["@everyone"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@everyone"]

[[land]]
ref = "main"
required_checks = ["test"]
"#;

#[derive(Default)]
struct TestPolicy(RwLock<Option<String>>);

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

struct World {
    forge: Arc<Forge>,
    ci: Arc<Ci>,
    svc: CiServices,
    changes: Arc<Changes>,
    bisector: Arc<Bisector>,
    vcs: Arc<HistoryVcs>,
    policy: Arc<TestPolicy>,
    org: Org,
    repo: Repo,
    work: tempfile::TempDir,
}

async fn world() -> World {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let meta = Arc::new(MemoryMetaStore::new());
    let vblobs = Arc::new(MemoryBlobStore::new(clock.clone()));
    let writer = Writer::new(meta.clone(), vblobs);
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
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let policy = Arc::new(TestPolicy::default());
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta, clock.clone())),
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
    let vcs = Arc::new(HistoryVcs::new(writer, clock.clone()));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    changes.attach(&forge);
    let ci = Ci::new(
        &forge,
        CiHooks {
            vcs: Arc::new(HistoryCiVcs::new(vcs.clone())),
            blobs: Arc::new(MemoryCiBlobs::new()),
            keys: Arc::new(HmacKey::new("k1", b"oidc-key")),
            secret_key: [3u8; 32],
            url_key: b"url-key".to_vec(),
            signer: Arc::new(bee_auth::KeyRing::new(
                bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap(),
            )),
            config: CiConfig {
                keepalive_ms: 50,
                green_poll_ms: 20,
                ..CiConfig::default()
            },
        },
    );
    ci.attach(&forge);
    let bisector = Bisector::new(
        &forge,
        ci.clone(),
        changes.clone(),
        BisectConfig {
            parallelism: 2,
            ..BisectConfig::default()
        },
    );
    bisector.attach(&forge);
    let svc = CiServices::new(forge.clone(), ci.clone());
    let cx = Cx::new("owner", PrincipalKind::Human);
    let name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
    let org = forge
        .orgs
        .create_org(&cx, &name, "", directory.get("owner").await.unwrap())
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    let repo = forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            NewRepo {
                id: forge.db.ids().ulid().unwrap(),
                name: "app".into(),
                visibility: Visibility::Private,
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    forge
        .hooks
        .provisioner
        .create_repo(&repo, &bee_forge::hooks::RepoInit::default())
        .await
        .unwrap();
    for who in ["writer", "agent"] {
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
        forge
            .orgs
            .add_collaborator(
                &cx,
                &repo.id,
                directory.get(who).await.unwrap(),
                RepoRole::Write,
                "",
            )
            .await
            .unwrap();
    }
    World {
        forge,
        ci,
        svc,
        changes,
        bisector,
        vcs,
        policy,
        org,
        repo,
        work: tempfile::tempdir().unwrap(),
    }
}

impl World {
    fn cx(&self, who: &str) -> Cx {
        let kind = PEOPLE
            .iter()
            .find(|(p, _)| *p == who)
            .map(|(_, k)| *k)
            .unwrap_or(PrincipalKind::Human);
        Cx::new(who, kind)
    }

    async fn tip_of(&self, name: &str) -> Option<ObjectId> {
        self.vcs
            .ref_tip(&self.repo, name)
            .await
            .unwrap()
            .and_then(|r| r.target)
    }

    async fn tip(&self) -> ObjectId {
        self.tip_of("main").await.unwrap()
    }

    async fn green(&self) -> Option<ObjectId> {
        self.forge.flush().await.unwrap();
        self.tip_of("green/main").await
    }

    async fn commit(
        &self,
        parent: Option<&ObjectId>,
        edits: &[(&str, Option<&str>)],
        message: &str,
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
                    author: "writer".into(),
                    creator: "writer".into(),
                    chain: vec!["writer".into()],
                    change_id: None,
                },
            )
            .await
            .unwrap()
    }

    /// Appends the `ref.updated` event the VCS bridge would for `main` at its current tip.
    async fn announce(&self) {
        let r = self.vcs.ref_tip(&self.repo, "main").await.unwrap().unwrap();
        let tip = r.target.clone().unwrap();
        self.forge
            .events
            .append(
                &Cx::system(),
                Event {
                    kind: "ref.updated".into(),
                    repo_id: self.repo.id.clone(),
                    org_id: self.repo.org_id.clone(),
                    principal: "writer".into(),
                    body: Some(event::Body::RefUpdated(Ref {
                        name: "main".into(),
                        target: Some(tip.clone()),
                        version: r.version,
                    })),
                    ..Default::default()
                },
                &format!("test:main:{}", hex(&tip.raw)),
            )
            .await
            .unwrap();
    }

    /// Commits onto `main`, moves it (no announcement).
    async fn push_quiet(&self, edits: &[(&str, Option<&str>)], message: &str) -> ObjectId {
        let parent = self.tip_of("main").await;
        let c = self.commit(parent.as_ref(), edits, message).await;
        self.vcs.set_ref(&self.repo, "main", &c).await.unwrap();
        c
    }

    /// Commits onto `main`, moves it and announces it.
    async fn push(&self, edits: &[(&str, Option<&str>)], message: &str) -> ObjectId {
        let c = self.push_quiet(edits, message).await;
        self.announce().await;
        c
    }

    async fn runner(&self, name: &str) -> LocalRunner {
        let cx = self.cx("owner");
        let owner = RunnerOwner::Repo(self.repo.id.clone(), self.org.id.clone());
        let (token, _) = self.ci.runner_token(&cx, &owner, "register").await.unwrap();
        let (runner, cred) = self
            .ci
            .register_runner(
                &Cx::system(),
                &token,
                name,
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
            Arc::new(InProcessApi::new(self.svc.clone(), runner.id, cred, vec![])),
            RunnerConfig {
                work_root: self.work.path().to_path_buf(),
                name: name.into(),
                heartbeat: Duration::from_millis(200),
                ..RunnerConfig::default()
            },
        )
    }

    /// Drives the runner, the event relay and the bisect and CI ticks until nothing runs.
    async fn settle(&self, runner: &LocalRunner) {
        for _ in 0..60 {
            runner.run_until_idle(100).await.unwrap();
            self.forge.flush().await.unwrap();
            self.bisector.tick(&Cx::system()).await.unwrap();
            let running = self
                .bisector
                .list(&self.cx("owner"), &self.repo, BisectState::Running)
                .await
                .unwrap();
            if running.is_empty() && runner.run_once().await.unwrap().is_none() {
                self.forge.flush().await.unwrap();
                return;
            }
        }
        panic!("CI did not settle");
    }

    async fn open(
        &self,
        base: &ObjectId,
        edits: &[(&str, Option<&str>)],
        title: &str,
    ) -> ChangeRequest {
        let head = self.commit(Some(base), edits, title).await;
        self.changes
            .open(
                &self.cx("writer"),
                &self.repo,
                NewChange {
                    head: Some(head),
                    title: title.into(),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap()
    }

    async fn land(&self, cr: &ChangeRequest) -> ChangeRequest {
        self.changes
            .land(
                &self.cx("writer"),
                &self.repo,
                &cr.id,
                LandMethod::Rebase,
                "",
                0,
            )
            .await
            .unwrap()
            .0
    }

    async fn report(&self, cs: &ObjectId, name: &str, status: CheckStatus) {
        self.forge
            .checks
            .report(
                &Cx::system(),
                &self.repo.org_id,
                &self.repo.id,
                Report {
                    changeset: Some(cs.clone()),
                    name: name.into(),
                    status,
                    post_land: true,
                    external_id: format!("external:{name}"),
                    ..Report::default()
                },
            )
            .await
            .unwrap();
        self.forge.flush().await.unwrap();
    }

    async fn read(&self, at: &ObjectId, path: &str) -> Option<String> {
        self.vcs
            .read_file(&self.repo, at, path)
            .await
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap())
    }

    async fn bisects(&self) -> Vec<Bisect> {
        self.bisector
            .list(&self.cx("owner"), &self.repo, BisectState::Unspecified)
            .await
            .unwrap()
    }

    async fn production(&self) {
        self.ci
            .create_environment(
                &self.cx("owner"),
                &self.repo,
                "production",
                &EnvironmentSpec {
                    require_green: Some(true),
                    ..EnvironmentSpec::default()
                },
            )
            .await
            .unwrap();
    }

    async fn deploy(&self, green_tip: bool) -> Result<Deployment> {
        self.ci
            .create_deployment(
                &self.cx("writer"),
                &self.repo,
                NewDeployment {
                    git_ref: "main".into(),
                    environment: "production".into(),
                    required_contexts: Some(vec![]),
                    deploy_green_tip: green_tip,
                    ..NewDeployment::default()
                },
            )
            .await
    }

    async fn auto_revert(&self) {
        self.forge
            .orgs
            .update_repo_settings(&self.cx("owner"), &self.repo.id, 0, |s| {
                s.auto_revert = true;
                Ok(())
            })
            .await
            .unwrap();
    }
}

/// Passes unless both `a.txt` and `b.txt` say 2: two changes that each pass alone.
const PAIR_WF: &str = "on:\n  push:\n  merge_group:\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: test \"$(cat a.txt)$(cat b.txt)\" != 22\n";

const VALUE_WF: &str = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: test \"$(cat value.txt)\" = good\n";

/// Runs a scenario on a runtime whose threads have room for the forge's debug-build futures:
/// the CI engine polls deep chains of large `async fn` frames inline (event listeners run
/// inside `EventLog::append`), which overflows the default 2 MiB test-thread stack in debug
/// builds. Release frames are a fraction of this.
fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(8 << 20)
                .enable_all()
                .build()
                .unwrap()
                .block_on(f)
        })
        .unwrap()
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
}

// ----- green/<ref> -----------------------------------------------------------------------------

#[test]
fn green_ref_moves_forward_only_to_green_tips() {
    run(green_ref_moves_forward_only_to_green_tips_body());
}

async fn green_ref_moves_forward_only_to_green_tips_body() {
    let w = world().await;
    let c1 = w.push_quiet(&[("f", Some("1"))], "c1").await;
    assert_eq!(w.green().await, None, "nothing tested yet");

    // Every latest post-land run passed: green/main moves to it.
    w.report(&c1, "test", CheckStatus::Passed).await;
    w.report(&c1, "e2e", CheckStatus::Passed).await;
    assert_eq!(w.green().await, Some(c1.clone()));

    // A failing run holds it.
    // (Reporters announce their checks queued before any finishes, as CI does when it
    // creates a run, so a later check can hold the tip.)
    let c2 = w.push_quiet(&[("f", Some("2"))], "c2").await;
    w.report(&c2, "test", CheckStatus::Queued).await;
    w.report(&c2, "e2e", CheckStatus::Queued).await;
    w.report(&c2, "test", CheckStatus::Passed).await;
    w.report(&c2, "e2e", CheckStatus::Failed).await;
    assert_eq!(w.green().await, Some(c1.clone()));

    // So does a pending one, until it passes.
    let c3 = w.push_quiet(&[("f", Some("3"))], "c3").await;
    w.report(&c3, "e2e", CheckStatus::Queued).await;
    w.report(&c3, "test", CheckStatus::Queued).await;
    w.report(&c3, "test", CheckStatus::Passed).await;
    assert_eq!(w.green().await, Some(c1.clone()));
    w.report(&c3, "e2e", CheckStatus::Running).await;
    assert_eq!(w.green().await, Some(c1.clone()));
    w.report(&c3, "e2e", CheckStatus::Passed).await;
    assert_eq!(w.green().await, Some(c3.clone()));

    // Never backwards: a late pass on an ancestor leaves it where it is.
    w.report(&c2, "e2e", CheckStatus::Passed).await;
    assert_eq!(w.green().await, Some(c3.clone()));

    // Never to a changeset that is not on the ref.
    let side = w.commit(Some(&c3), &[("f", Some("side"))], "side").await;
    w.report(&side, "test", CheckStatus::Passed).await;
    assert_eq!(w.green().await, Some(c3.clone()));

    // Repeating the decision is a no-op.
    assert_eq!(
        w.ci.advance_green(&Cx::system(), &w.repo, &c3)
            .await
            .unwrap(),
        None
    );
    let st =
        w.ci.green_status(&w.cx("owner"), &w.repo, "main", None)
            .await
            .unwrap();
    assert_eq!(st.green_tip, Some(c3));
    assert_eq!(st.green_ref, "green/main");
}

// ----- require_green on workflow deploys -------------------------------------------------------

const DEPLOY_WF: &str = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: \"true\"\n  deploy:\n    needs: test\n    runs-on: ubuntu-latest\n    environment: production\n    steps:\n      - run: echo deployed\n";

async fn deploy_job(w: &World) -> WorkflowJob {
    let runs =
        w.ci.list_runs(&w.cx("owner"), &w.repo.id, "")
            .await
            .unwrap();
    let tip = w.tip().await;
    let run = runs
        .iter()
        .find(|r| r.head_changeset.as_ref() == Some(&tip))
        .expect("a run of the tip");
    w.ci.list_jobs(&w.cx("owner"), &w.repo.id, &run.id, 0, false)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.name == "deploy")
        .expect("deploy job")
}

#[test]
fn require_green_holds_and_refuses_workflow_deploys() {
    run(require_green_holds_and_refuses_workflow_deploys_body());
}

async fn require_green_holds_and_refuses_workflow_deploys_body() {
    let w = world().await;
    w.production().await;
    let runner = w.runner("r").await;

    // The deploying run's own `test` passed; its own deploy job is not waited for.
    w.push(&[(".github/workflows/cd.yml", Some(DEPLOY_WF))], "cd")
        .await;
    w.settle(&runner).await;
    let job = deploy_job(&w).await;
    assert_eq!(
        job.conclusion,
        WorkflowConclusion::Success as i32,
        "{job:?}"
    );

    // Another post-land check still running holds the deploy until it passes.
    let tip = w.push_quiet(&[("f", Some("1"))], "one").await;
    w.report(&tip, "e2e", CheckStatus::Running).await;
    w.announce().await;
    w.settle(&runner).await;
    let job = deploy_job(&w).await;
    assert_eq!(job.status, WorkflowStatus::Waiting as i32, "{job:?}");
    w.report(&tip, "e2e", CheckStatus::Passed).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    w.ci.tick(&Cx::system()).await.unwrap();
    w.settle(&runner).await;
    let job = deploy_job(&w).await;
    assert_eq!(
        job.conclusion,
        WorkflowConclusion::Success as i32,
        "{job:?}"
    );

    // A running bisect of the ref holds it too.
    let before = w.tip().await;
    w.push_quiet(&[("g", Some("2"))], "two (a)").await;
    let tip = w.push_quiet(&[("f", Some("2"))], "two (b)").await;
    let bisect = w
        .bisector
        .start(
            &w.cx("writer"),
            &w.repo,
            StartBisect {
                check_name: "manual".into(),
                good: Some(before),
                manual: true,
                ..StartBisect::default()
            },
        )
        .await
        .unwrap();
    w.announce().await;
    runner.run_until_idle(100).await.unwrap();
    w.forge.flush().await.unwrap();
    let job = deploy_job(&w).await;
    assert_eq!(job.status, WorkflowStatus::Waiting as i32, "{job:?}");
    w.bisector
        .cancel(&w.cx("writer"), &w.repo, &bisect.id)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    w.ci.tick(&Cx::system()).await.unwrap();
    w.settle(&runner).await;
    let job = deploy_job(&w).await;
    assert_eq!(
        job.conclusion,
        WorkflowConclusion::Success as i32,
        "{job:?}"
    );
    assert_eq!(w.green().await, Some(tip));

    // A failed post-land check refuses it.
    let tip = w.push_quiet(&[("f", Some("3"))], "three").await;
    w.report(&tip, "e2e", CheckStatus::Failed).await;
    w.announce().await;
    w.settle(&runner).await;
    let job = deploy_job(&w).await;
    assert_eq!(
        job.conclusion,
        WorkflowConclusion::Failure as i32,
        "{job:?}"
    );
    let deployments =
        w.ci.list_deployments(&w.cx("owner"), &w.repo, "production")
            .await
            .unwrap();
    assert!(
        deployments
            .iter()
            .all(|d| d.changeset.as_ref() != Some(&tip)),
        "no deployment of a failing tip"
    );
}

// ----- confirm before blame ----------------------------------------------------------------------

#[test]
fn a_failure_that_passes_on_re_run_is_flaky_and_reverts_nothing() {
    run(a_failure_that_passes_on_re_run_is_flaky_and_reverts_nothing_body());
}

async fn a_failure_that_passes_on_re_run_is_flaky_and_reverts_nothing_body() {
    let w = world().await;
    w.auto_revert().await;
    let flag = w.work.path().join("flaky-flag");
    // Fails the first time it runs after the flag is armed, passes after.
    let wf = format!(
        "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: \"if [ -f '{0}.armed' ] && [ ! -f '{0}' ]; then touch '{0}'; exit 1; fi\"\n",
        flag.display()
    );
    let runner = w.runner("r").await;
    let base = w
        .push(&[(".github/workflows/ci.yml", Some(wf.as_str()))], "ci")
        .await;
    w.settle(&runner).await;
    assert_eq!(w.green().await, Some(base.clone()));

    std::fs::write(format!("{}.armed", flag.display()), b"").unwrap();
    let cr = w.open(&base, &[("f.txt", Some("x"))], "Harmless").await;
    let landed = w.land(&cr).await;
    w.announce().await;
    w.settle(&runner).await;

    let all = w.bisects().await;
    assert_eq!(all.len(), 1, "{all:?}");
    let b = &all[0];
    assert_eq!(b.state, BisectState::Flaky as i32, "{b:?}");
    assert!(b.revert_change_request_id.is_empty());
    assert!(b.first_bad.is_none());
    let session = w
        .bisector
        .session(&w.cx("owner"), &w.repo, &b.id)
        .await
        .unwrap();
    assert!(session.note.contains("flaky"), "{}", session.note);
    assert_eq!(session.probes.len(), 1, "only the confirmation ran");
    assert_eq!(session.probes[0].source, "confirm");
    let got = w
        .changes
        .get(&w.cx("owner"), &w.repo, &landed.id)
        .await
        .unwrap();
    assert_eq!(got.state, ChangeState::Landed as i32);
    assert_eq!(
        w.read(&w.tip().await, "f.txt").await.as_deref(),
        Some("x"),
        "nothing was reverted"
    );
    let kinds: Vec<String> = w
        .forge
        .events
        .read(&w.cx("owner"), &w.repo.id, 0, &[], 10_000)
        .await
        .unwrap()
        .0
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.iter().any(|k| k == "bisect.flaky"), "{kinds:?}");
    assert!(!kinds.iter().any(|k| k == "bisect.reverted"));
    // The flaky tip never became green.
    assert_eq!(w.green().await, Some(base));
}

// ----- a culprit mid-stack ---------------------------------------------------------------------

#[test]
fn a_culprit_in_the_middle_of_a_stack_reverts_its_change_request() {
    run(a_culprit_in_the_middle_of_a_stack_reverts_its_change_request_body());
}

async fn a_culprit_in_the_middle_of_a_stack_reverts_its_change_request_body() {
    let w = world().await;
    w.auto_revert().await;
    let runner = w.runner("r").await;
    let base = w
        .push(
            &[
                ("value.txt", Some("good")),
                (".github/workflows/ci.yml", Some(VALUE_WF)),
            ],
            "ci",
        )
        .await;
    w.settle(&runner).await;

    // A three-changeset CR whose middle changeset breaks the test.
    let s1 = w.commit(Some(&base), &[("s1", Some("x"))], "step 1").await;
    let s2 = w
        .commit(Some(&s1), &[("value.txt", Some("bad"))], "step 2")
        .await;
    let s3 = w.commit(Some(&s2), &[("s3", Some("x"))], "step 3").await;
    let stack = w
        .changes
        .open(
            &w.cx("writer"),
            &w.repo,
            NewChange {
                head: Some(s3),
                title: "Three steps".into(),
                ..NewChange::default()
            },
        )
        .await
        .unwrap();
    // Another CR on top moves the tip past the stack.
    let other = w.open(&base, &[("other", Some("x"))], "Other").await;
    let stack = w.land(&stack).await;
    w.land(&other).await;
    w.announce().await;
    w.settle(&runner).await;

    let all = w.bisects().await;
    assert_eq!(all.len(), 1, "{all:?}");
    let b = &all[0];
    assert_eq!(b.state, BisectState::Found as i32, "{b:?}");
    let first_bad = b.first_bad.clone().unwrap();
    assert_ne!(
        Some(&first_bad),
        stack.landed_ref.as_ref().and_then(|r| r.target.as_ref()),
        "the culprit is not the stack's final landed changeset"
    );
    assert_eq!(
        w.read(&first_bad, "value.txt").await.as_deref(),
        Some("bad")
    );
    let session = w
        .bisector
        .session(&w.cx("owner"), &w.repo, &b.id)
        .await
        .unwrap();
    assert_eq!(
        session.culprit_change_request_id, stack.id,
        "{}",
        session.note
    );
    assert!(session.revert_landed, "{}", session.note);
    let revert = w
        .changes
        .get(&w.cx("owner"), &w.repo, &b.revert_change_request_id)
        .await
        .unwrap();
    assert_eq!(revert.revert_of, stack.id);
    let tip = w.tip().await;
    assert_eq!(w.read(&tip, "value.txt").await.as_deref(), Some("good"));
    assert!(w.read(&tip, "other").await.is_some(), "the other CR stays");
}

// ----- two CRs that pass alone and fail together -----------------------------------------------

#[test]
fn a_breaking_combination_is_reverted_and_deploys_stay_on_the_last_green_tip() {
    run(a_breaking_combination_is_reverted_and_deploys_stay_on_the_last_green_tip_body());
}

async fn a_breaking_combination_is_reverted_and_deploys_stay_on_the_last_green_tip_body() {
    let w = world().await;
    w.auto_revert().await;
    w.production().await;
    let runner = w.runner("r").await;
    let base = w
        .push(
            &[
                ("a.txt", Some("1")),
                ("b.txt", Some("1")),
                (".github/workflows/ci.yml", Some(PAIR_WF)),
            ],
            "ci",
        )
        .await;
    w.settle(&runner).await;
    assert_eq!(w.green().await, Some(base.clone()));

    // Both CRs are written against the same base; each passes alone.
    let a = w.open(&base, &[("a.txt", Some("2"))], "A").await;
    let b = w.open(&base, &[("b.txt", Some("2"))], "B").await;
    let a = w.land(&a).await;
    let a_tip = w.tip().await;
    w.announce().await;
    w.settle(&runner).await;
    assert_eq!(w.green().await, Some(a_tip.clone()));

    // B lands on top of A, then an unrelated change: the combination fails post-land.
    let b = w.land(&b).await;
    let b_tip = w.tip().await;
    let c = w.open(&b_tip, &[("c.txt", Some("x"))], "C").await;
    w.land(&c).await;
    let bad_tip = w.tip().await;
    w.announce().await;

    // Before the bisect finishes, deploys stay on the last green tip.
    runner.run_until_idle(100).await.unwrap();
    w.forge.flush().await.unwrap();
    assert_eq!(w.green().await, Some(a_tip.clone()));
    let refused = w.deploy(false).await.unwrap_err();
    assert_eq!(refused.reason, ErrorReason::PolicyDenied, "{refused:?}");
    let dep = w.deploy(true).await.unwrap();
    assert_eq!(dep.changeset, Some(a_tip.clone()), "the newest green tip");

    w.settle(&runner).await;
    let all = w.bisects().await;
    assert_eq!(all.len(), 1, "{all:?}");
    let bisect = &all[0];
    assert_eq!(bisect.state, BisectState::Found as i32, "{bisect:?}");
    assert_eq!(bisect.first_bad, Some(b_tip.clone()));
    assert_eq!(bisect.good, Some(a_tip.clone()));
    let session = w
        .bisector
        .session(&w.cx("owner"), &w.repo, &bisect.id)
        .await
        .unwrap();
    assert_eq!(
        session.culprit_change_request_id, b.id,
        "B is blamed, not A"
    );
    assert!(session.revert_landed, "{}", session.note);
    assert_eq!(session.probes[0].source, "confirm");
    // green/main never pointed at a failing tip.
    let green = w.green().await.unwrap();
    assert_ne!(green, b_tip);
    assert_ne!(green, bad_tip);

    // The revert's post-land run passes; green/main moves to it.
    w.announce().await;
    w.settle(&runner).await;
    let tip = w.tip().await;
    assert_eq!(w.read(&tip, "a.txt").await.as_deref(), Some("2"), "A stays");
    assert_eq!(
        w.read(&tip, "b.txt").await.as_deref(),
        Some("1"),
        "B is reverted"
    );
    assert_eq!(w.read(&tip, "c.txt").await.as_deref(), Some("x"), "C stays");
    assert_eq!(w.green().await, Some(tip.clone()));
    let dep = w.deploy(false).await.unwrap();
    assert_eq!(dep.changeset, Some(tip));
    let got = w.changes.get(&w.cx("owner"), &w.repo, &a.id).await.unwrap();
    assert_eq!(got.state, ChangeState::Landed as i32);
}

#[test]
fn the_combined_merge_queue_ejects_the_second_change_before_it_lands() {
    run(the_combined_merge_queue_ejects_the_second_change_before_it_lands_body());
}

async fn the_combined_merge_queue_ejects_the_second_change_before_it_lands_body() {
    let w = world().await;
    *w.policy.0.write().unwrap() = Some(REQUIRE_TEST.into());
    w.forge
        .orgs
        .update_repo_settings(&w.cx("owner"), &w.repo.id, 0, |s| {
            s.merge_queue_required = true;
            s.merge_queue_check_combined = true;
            Ok(())
        })
        .await
        .unwrap();
    let runner = w.runner("r").await;
    let base = w
        .push(
            &[
                ("a.txt", Some("1")),
                ("b.txt", Some("1")),
                (".github/workflows/ci.yml", Some(PAIR_WF)),
            ],
            "ci",
        )
        .await;
    w.settle(&runner).await;

    let a = w.open(&base, &[("a.txt", Some("2"))], "A").await;
    let b = w.open(&base, &[("b.txt", Some("2"))], "B").await;
    for cr in [&a, &b] {
        w.changes
            .enqueue(&w.cx("writer"), &w.repo, &cr.id, LandMethod::Rebase, 0)
            .await
            .unwrap();
    }

    // A's combined result is built on the tip and checked before anything lands.
    let step = w
        .changes
        .step_queue(&Cx::system(), &w.repo, "main")
        .await
        .unwrap();
    assert_eq!(step, QueueStep::Waiting);
    let entry = w
        .changes
        .get(&w.cx("owner"), &w.repo, &a.id)
        .await
        .unwrap()
        .merge_queue_entry
        .unwrap();
    assert_eq!(entry.state, MergeQueueState::Checking as i32);
    let spec_a = entry.speculative_head.clone().unwrap();
    assert_eq!(w.tip().await, base, "nothing landed yet");

    let mut steps = vec![step];
    for _ in 0..20 {
        w.settle(&runner).await;
        let step = w
            .changes
            .step_queue(&Cx::system(), &w.repo, "main")
            .await
            .unwrap();
        let done = step == QueueStep::Empty;
        steps.push(step);
        if done {
            break;
        }
    }
    assert!(
        steps
            .iter()
            .any(|s| matches!(s, QueueStep::Landed(cr) if cr.id == a.id)),
        "{steps:?}"
    );
    assert!(
        steps.iter().any(
            |s| matches!(s, QueueStep::Ejected(id, reason) if *id == b.id && reason == "POLICY_DENIED")
        ),
        "{steps:?}"
    );
    // A landed exactly what was tested; B never landed.
    let tip = w.tip().await;
    assert_eq!(tip, spec_a);
    assert_eq!(w.read(&tip, "a.txt").await.as_deref(), Some("2"));
    assert_eq!(w.read(&tip, "b.txt").await.as_deref(), Some("1"));
    let got_b = w.changes.get(&w.cx("owner"), &w.repo, &b.id).await.unwrap();
    assert_eq!(got_b.state, ChangeState::Open as i32);
    assert!(got_b.merge_queue_entry.is_none());
    let (comments, _) = w
        .changes
        .comments(&w.cx("owner"), &w.repo, &b.id, "", 100)
        .await
        .unwrap();
    assert!(
        comments.iter().any(|c| c.body.contains("test")),
        "the ejection names the failing check: {comments:?}"
    );
    let kinds: Vec<String> = w
        .forge
        .events
        .read(&w.cx("owner"), &w.repo.id, 0, &[], 10_000)
        .await
        .unwrap()
        .0
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "merge_queue.checks_requested")
            .count(),
        2,
        "{kinds:?}"
    );
    // Both combined results ran the merge_group workflow.
    let runs =
        w.ci.list_runs(&w.cx("owner"), &w.repo.id, "")
            .await
            .unwrap();
    assert_eq!(
        runs.iter().filter(|r| r.event == "merge_group").count(),
        2,
        "{runs:?}"
    );
    assert!(w.bisects().await.is_empty(), "nothing broke main");
}
