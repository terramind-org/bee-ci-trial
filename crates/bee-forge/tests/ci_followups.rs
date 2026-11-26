//! CI follow-ups end to end over real history (`HistoryVcs` on in-memory VCS stores), the
//! changes domain and the local runner: sealed-box secrets through `SecretService`,
//! environments, deployments and protection rules through `DeploymentService`, post-land
//! bisect and automatic revert (AS-CI-02), manual bisect sessions, remote composite and
//! JavaScript actions through the action registry, and `pull_request_review` triggers.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bee_auth::KeyRing;
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::bisect::service::BisectCheckService;
use bee_forge::bisect::{BisectConfig, Bisector, Outcome, StartBisect};
use bee_forge::changes::vcs::{ChangeVcs, LandFacts, NewCommit};
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams, NewChange};
use bee_forge::checks::Report;
use bee_forge::ci::model::JobRecord;
use bee_forge::ci::policy::PolicyScope;
use bee_forge::ci::runner::{
    ActionRegistry, ForgeActionSource, InProcessApi, LocalRunner, Ran, RunnerConfig,
};
use bee_forge::ci::runners::RunnerOwner;
use bee_forge::ci::sealed::{open, public_key_of, seal, seal_with_ephemeral};
use bee_forge::ci::secrets::VaultScope;
use bee_forge::ci::{Ci, CiConfig, CiHooks, CiServices, HistoryCiVcs, HmacKey, MemoryCiBlobs};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PrincipalDirectory};
use bee_forge::ids::{Clock, SystemClock, hex};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::{Key, Scan};
use bee_forge::{ForgeError, Result};
use bee_proto::v1::check_service_server::CheckService;
use bee_proto::v1::deployment_service_server::DeploymentService;
use bee_proto::v1::secret_service_server::SecretService;
use bee_proto::v1::workflow_service_server::WorkflowService;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

// ----- fixture -------------------------------------------------------------------------------

const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("admin", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("agent", PrincipalKind::Agent),
    ("deploy-bot", PrincipalKind::Service),
];

struct World {
    forge: Arc<Forge>,
    ci: Arc<Ci>,
    svc: CiServices,
    changes: Arc<Changes>,
    bisector: Arc<Bisector>,
    vcs: Arc<HistoryVcs>,
    directory: Arc<MemoryDirectory>,
    ring: KeyRing,
    clock: Arc<dyn Clock>,
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
    let ring = KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta, clock.clone())),
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
    let w = World {
        forge,
        ci,
        svc,
        changes,
        bisector,
        vcs,
        directory,
        ring,
        clock,
        org: org.clone(),
        repo: Repo::default(),
        work: tempfile::tempdir().unwrap(),
    };
    let repo = w.new_repo(&org, "app", Visibility::Private).await;
    w.forge
        .orgs
        .set_member(
            &cx,
            &org.id,
            w.directory.get("admin").await.unwrap(),
            Role::Admin,
        )
        .await
        .unwrap();
    for who in ["writer", "reader", "agent", "deploy-bot"] {
        w.forge
            .orgs
            .set_member(
                &cx,
                &org.id,
                w.directory.get(who).await.unwrap(),
                Role::Member,
            )
            .await
            .unwrap();
    }
    for (who, role) in [
        ("writer", RepoRole::Write),
        ("agent", RepoRole::Write),
        ("deploy-bot", RepoRole::Read),
    ] {
        w.forge
            .orgs
            .add_collaborator(&cx, &repo.id, w.directory.get(who).await.unwrap(), role, "")
            .await
            .unwrap();
    }
    World { repo, ..w }
}

impl World {
    async fn new_repo(&self, org: &Org, name: &str, visibility: Visibility) -> Repo {
        let cx = Cx::new("owner", PrincipalKind::Human);
        let repo = self
            .forge
            .orgs
            .create_repo(
                &cx,
                &org.id,
                NewRepo {
                    id: self.forge.db.ids().ulid().unwrap(),
                    name: name.into(),
                    visibility,
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        self.forge
            .hooks
            .provisioner
            .create_repo(&repo, &bee_forge::hooks::RepoInit::default())
            .await
            .unwrap();
        repo
    }

    fn cx(&self, who: &str) -> Cx {
        let kind = PEOPLE
            .iter()
            .find(|(p, _)| *p == who)
            .map(|(_, k)| *k)
            .unwrap_or(PrincipalKind::Human);
        Cx::new(who, kind)
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

    fn rr(&self) -> Option<RepoRef> {
        Some(RepoRef {
            repo: Some(repo_ref::Repo::Id(self.repo.id.clone())),
        })
    }

    async fn tip_of(&self, repo: &Repo, name: &str) -> Option<ObjectId> {
        self.vcs
            .ref_tip(repo, name)
            .await
            .unwrap()
            .and_then(|r| r.target)
    }

    async fn tip(&self) -> ObjectId {
        self.tip_of(&self.repo, "main").await.unwrap()
    }

    async fn commit_in(
        &self,
        repo: &Repo,
        parent: Option<&ObjectId>,
        edits: &[(&str, Option<&str>)],
        message: &str,
        who: &str,
    ) -> ObjectId {
        self.vcs
            .create_commit(
                repo,
                NewCommit {
                    parent: parent.cloned(),
                    edits: edits
                        .iter()
                        .map(|(p, c)| (p.to_string(), c.map(|s| s.as_bytes().to_vec())))
                        .collect(),
                    message: message.to_string(),
                    author: who.into(),
                    creator: who.into(),
                    chain: vec![who.into()],
                    change_id: None,
                },
            )
            .await
            .unwrap()
    }

    /// Appends the `ref.updated` event the VCS bridge would for `main` at its current tip.
    async fn announce(&self, repo: &Repo) {
        let tip = self.tip_of(repo, "main").await.unwrap();
        let version = self
            .vcs
            .ref_tip(repo, "main")
            .await
            .unwrap()
            .map(|r| r.version)
            .unwrap_or(1);
        self.forge
            .events
            .append(
                &Cx::system(),
                Event {
                    kind: "ref.updated".into(),
                    repo_id: repo.id.clone(),
                    org_id: repo.org_id.clone(),
                    principal: "writer".into(),
                    body: Some(event::Body::RefUpdated(Ref {
                        name: "main".into(),
                        target: Some(tip.clone()),
                        version,
                    })),
                    ..Default::default()
                },
                &format!("test:main:{}", hex(&tip.raw)),
            )
            .await
            .unwrap();
    }

    /// Commits onto `main` of the app repo, moves it and announces it.
    async fn push(&self, edits: &[(&str, Option<&str>)], message: &str) -> ObjectId {
        let parent = self.tip_of(&self.repo, "main").await;
        let c = self
            .commit_in(&self.repo, parent.as_ref(), edits, message, "writer")
            .await;
        self.vcs.set_ref(&self.repo, "main", &c).await.unwrap();
        self.announce(&self.repo).await;
        c
    }

    async fn runner(&self, name: &str, actions: Option<ActionRegistry>) -> LocalRunner {
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
                actions,
                ..RunnerConfig::default()
            },
        )
    }

    async fn drain(&self, runner: &LocalRunner) -> Vec<Ran> {
        runner.run_until_idle(100).await.unwrap()
    }

    async fn runs(&self) -> Vec<WorkflowRun> {
        self.ci
            .list_runs(&self.cx("owner"), &self.repo.id, "")
            .await
            .unwrap()
    }

    async fn jobs(&self, run: &str) -> Vec<WorkflowJob> {
        self.ci
            .list_jobs(&self.cx("owner"), &self.repo.id, run, 0, false)
            .await
            .unwrap()
    }

    async fn log(&self, job: &str) -> String {
        String::from_utf8(
            self.ci
                .job_log_bytes(&self.cx("owner"), &self.repo.id, job)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    async fn kinds(&self) -> Vec<Event> {
        self.forge.flush().await.unwrap();
        self.forge
            .events
            .read(&self.cx("owner"), &self.repo.id, 0, &[], 10_000)
            .await
            .unwrap()
            .0
    }
}

fn st<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> (String, String) {
    let s = r.expect_err("expected failure");
    let d = bee_proto::errors::detail_from_status(&s).expect("error detail");
    (d.reason, d.subcode)
}

fn repo_scope(w: &World) -> Option<SecretScope> {
    Some(SecretScope {
        kind: SecretScopeKind::Repo as i32,
        repo: w.rr(),
        ..Default::default()
    })
}

// ----- sealed boxes and SecretService --------------------------------------------------------

/// A known-answer vector generated with this implementation (recipient secret key 0x01..0x20,
/// ephemeral secret key 0x21..0x40) and frozen: sealing must stay byte-for-byte stable and
/// libsodium-compatible openers must keep accepting it.
const KAT_SEALED: &str = "5869aff450549732cbaaed5e5df9b30a6da31cb0e5742bad5ad4a1a768f1a67bb08ec93222538ae7f07195c3fc38fab431afff79ed7e3f1209b89f890cbea1282740e2";

#[test]
fn sealed_box_known_answer_vector() {
    let recipient: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
    let ephemeral: [u8; 32] = std::array::from_fn(|i| i as u8 + 0x21);
    let pk = public_key_of(recipient);
    let boxed = seal_with_ephemeral(&pk, ephemeral, b"swarm sealed secret").unwrap();
    assert_eq!(hex(&boxed), KAT_SEALED);
    assert_eq!(&boxed[..32], &public_key_of(ephemeral));
    assert_eq!(boxed.len(), 32 + 16 + 19);
    assert_eq!(open(recipient, &boxed).unwrap(), b"swarm sealed secret");
    let random = seal(&pk, b"swarm sealed secret").unwrap();
    assert_ne!(random, boxed);
    assert_eq!(open(recipient, &random).unwrap(), b"swarm sealed secret");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_service_takes_sealed_boxes_and_never_returns_values() {
    let w = world().await;
    let key = w
        .svc
        .get_secret_public_key(common::req(
            &w.token("writer"),
            GetSecretPublicKeyRequest {
                scope: repo_scope(&w),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(key.key.len(), 32);
    let pk: [u8; 32] = key.key.clone().try_into().unwrap();
    let sealed = seal(&pk, b"hunter2-value").unwrap();
    let put = |who: &str, value: Vec<u8>, key_id: String| {
        w.svc.put_secret(common::req(
            &w.token(who),
            PutSecretRequest {
                scope: repo_scope(&w),
                name: "deploy_token".into(),
                encrypted_value: value,
                key_id,
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        st(put("writer", sealed.clone(), key.key_id.clone()).await).0,
        "PERMISSION_DENIED",
        "writing secrets needs admin"
    );
    let s = put("owner", sealed.clone(), key.key_id.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(s.name, "DEPLOY_TOKEN");
    assert_eq!(
        st(put("owner", sealed.clone(), "1-0000".into()).await),
        ("INVALID_ARGUMENT".into(), "stale_key_id".into())
    );
    let mut tampered = sealed.clone();
    tampered[40] ^= 0xff;
    assert_eq!(
        st(put("owner", tampered, key.key_id.clone()).await),
        ("INVALID_ARGUMENT".into(), "bad_ciphertext".into())
    );
    let other_repo_key =
        w.ci.secret_public_key(&VaultScope::Org(w.org.id.clone()))
            .await
            .unwrap();
    let wrong_scope = seal(&other_repo_key.public_key, b"x").unwrap();
    assert_eq!(
        st(put("owner", wrong_scope, key.key_id.clone()).await).1,
        "bad_ciphertext",
        "a box sealed for another scope does not open"
    );
    let listed = w
        .svc
        .list_secrets(common::req(
            &w.token("writer"),
            ListSecretsRequest {
                scope: repo_scope(&w),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.secrets.len(), 1);
    let debug = format!("{listed:?}");
    assert!(!debug.contains("hunter2"), "{debug}");
    assert_eq!(
        st(w.svc
            .list_secrets(common::req(
                &w.token("reader"),
                ListSecretsRequest {
                    scope: repo_scope(&w),
                    page: None,
                },
            ))
            .await)
        .0,
        "PERMISSION_DENIED"
    );
    // The value reaches a job and nothing else.
    let values = w.ci.secrets_for(&w.cx("owner"), &w.repo, "").await.unwrap();
    assert_eq!(values["DEPLOY_TOKEN"], "hunter2-value");

    // Org scope with selected repos.
    let org_scope = Some(SecretScope {
        kind: SecretScopeKind::Org as i32,
        org: w.org.name.clone(),
        ..Default::default()
    });
    let ok = w
        .svc
        .get_secret_public_key(common::req(
            &w.token("owner"),
            GetSecretPublicKeyRequest {
                scope: org_scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let opk: [u8; 32] = ok.key.clone().try_into().unwrap();
    w.svc
        .put_secret(common::req(
            &w.token("owner"),
            PutSecretRequest {
                scope: org_scope.clone(),
                name: "ORG_TOKEN".into(),
                encrypted_value: seal(&opk, b"org-value").unwrap(),
                key_id: ok.key_id.clone(),
                visibility: SecretVisibility::Selected as i32,
                selected_repos: vec![w.rr().unwrap()],
            },
        ))
        .await
        .unwrap();
    let repo_view = w
        .svc
        .list_repo_org_secrets(common::req(
            &w.token("writer"),
            ListRepoOrgSecretsRequest {
                repo: w.rr(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(repo_view.secrets[0].name, "ORG_TOKEN");
    w.svc
        .remove_secret_selected_repo(common::req(
            &w.token("owner"),
            RemoveSecretSelectedRepoRequest {
                scope: org_scope.clone(),
                name: "ORG_TOKEN".into(),
                selected_repo: w.rr(),
            },
        ))
        .await
        .unwrap();
    assert!(
        w.svc
            .list_repo_org_secrets(common::req(
                &w.token("writer"),
                ListRepoOrgSecretsRequest {
                    repo: w.rr(),
                    page: None,
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .secrets
            .is_empty()
    );
    assert_eq!(
        st(w.svc
            .get_secret_public_key(common::req(
                &w.token("writer"),
                GetSecretPublicKeyRequest { scope: org_scope },
            ))
            .await)
        .0,
        "PERMISSION_DENIED",
        "org secrets are for org admins"
    );

    // Environment scope: the environment must exist; deleting it deletes its secrets.
    let env_scope = Some(SecretScope {
        kind: SecretScopeKind::Environment as i32,
        repo: w.rr(),
        environment: "production".into(),
        ..Default::default()
    });
    assert_eq!(
        st(w.svc
            .get_secret_public_key(common::req(
                &w.token("owner"),
                GetSecretPublicKeyRequest {
                    scope: env_scope.clone(),
                },
            ))
            .await)
        .0,
        "NOT_FOUND"
    );
    w.svc
        .create_deployment_environment(common::req(
            &w.token("owner"),
            CreateDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "production".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let ek = w
        .svc
        .get_secret_public_key(common::req(
            &w.token("owner"),
            GetSecretPublicKeyRequest {
                scope: env_scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_ne!(ek.key, key.key, "each scope has its own key");
    let epk: [u8; 32] = ek.key.clone().try_into().unwrap();
    w.svc
        .put_secret(common::req(
            &w.token("owner"),
            PutSecretRequest {
                scope: env_scope.clone(),
                name: "PROD_ONLY".into(),
                encrypted_value: seal(&epk, b"prod-value").unwrap(),
                key_id: ek.key_id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let with_env =
        w.ci.secrets_for(&w.cx("owner"), &w.repo, "production")
            .await
            .unwrap();
    assert_eq!(with_env["PROD_ONLY"], "prod-value");
    w.svc
        .delete_deployment_environment(common::req(
            &w.token("owner"),
            DeleteDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "production".into(),
            },
        ))
        .await
        .unwrap();
    let after =
        w.ci.secrets_for(&w.cx("owner"), &w.repo, "production")
            .await
            .unwrap();
    assert!(!after.contains_key("PROD_ONLY"));

    // Workspace-user and agent scopes belong elsewhere.
    let (reason, sub) = st(w
        .svc
        .list_secrets(common::req(
            &w.token("owner"),
            ListSecretsRequest {
                scope: Some(SecretScope {
                    kind: SecretScopeKind::Agent as i32,
                    principal_id: "agent".into(),
                    ..Default::default()
                }),
                page: None,
            },
        ))
        .await);
    assert_eq!(
        (reason.as_str(), sub.as_str()),
        ("UNIMPLEMENTED", "served_elsewhere")
    );

    // Variables round-trip and are readable.
    w.svc
        .create_variable(common::req(
            &w.token("owner"),
            CreateVariableRequest {
                scope: repo_scope(&w),
                name: "REGION".into(),
                value: "eu-west".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let v = w
        .svc
        .get_variable(common::req(
            &w.token("writer"),
            GetVariableRequest {
                scope: repo_scope(&w),
                name: "region".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(v.value, "eu-west");
}

// ----- DeploymentService ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deployment_service_environments_protection_and_statuses() {
    let w = world().await;
    let c1 = w.push(&[("README.md", Some("# app\n"))], "init").await;
    let reviewer = DeploymentEnvironmentReviewer {
        kind: DeploymentReviewerKind::Principal as i32,
        principal: Some(Principal {
            id: "admin".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        st(w.svc
            .create_deployment_environment(common::req(
                &w.token("writer"),
                CreateDeploymentEnvironmentRequest {
                    repo: w.rr(),
                    name: "production".into(),
                    ..Default::default()
                },
            ))
            .await)
        .0,
        "PERMISSION_DENIED",
        "environments are admin settings"
    );
    w.svc
        .create_deployment_environment(common::req(
            &w.token("owner"),
            CreateDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "production".into(),
                reviewers: vec![reviewer],
                prevent_self_review: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let create = |who: &str, env: &str, contexts: Option<Vec<String>>| {
        w.svc.create_deployment(common::req(
            &w.token(who),
            CreateDeploymentRequest {
                repo: w.rr(),
                r#ref: "main".into(),
                environment: env.into(),
                required_contexts: contexts.map(|c| DeploymentRequiredContexts { contexts: c }),
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        st(create("reader", "production", None).await).0,
        "PERMISSION_DENIED"
    );
    assert_eq!(
        st(create("writer", "production", None).await).0,
        "APPROVAL_REQUIRED",
        "created, but waiting on the required reviewer"
    );
    let pending = w
        .svc
        .list_pending_deployment_reviews(common::req(
            &w.token("admin"),
            ListPendingDeploymentReviewsRequest {
                repo: w.rr(),
                environment: "production".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .reviews;
    assert_eq!(pending.len(), 1);
    assert!(pending[0].caller_can_approve);
    let dep_id = pending[0].deployment_id.clone();
    let review = |who: &str, d: DeploymentReviewDecision| {
        w.svc.review_pending_deployment(common::req(
            &w.token(who),
            ReviewPendingDeploymentRequest {
                repo: w.rr(),
                deployment_id: dep_id.clone(),
                decision: d as i32,
                comment: "ok".into(),
            },
        ))
    };
    assert_eq!(
        st(review("writer", DeploymentReviewDecision::Approved).await).0,
        "PERMISSION_DENIED"
    );
    let approved = review("admin", DeploymentReviewDecision::Approved)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(approved.state, DeploymentState::Queued as i32);
    assert_eq!(approved.changeset, Some(c1.clone()));
    assert_eq!(
        st(review("admin", DeploymentReviewDecision::Approved).await).0,
        "INVALID_STATE"
    );
    let status = |who: &str, id: &str, s: DeploymentState, url: &str| {
        w.svc.create_deployment_status(common::req(
            &w.token(who),
            CreateDeploymentStatusRequest {
                repo: w.rr(),
                deployment_id: id.into(),
                state: s as i32,
                environment_url: url.into(),
                auto_inactive: true,
                ..Default::default()
            },
        ))
    };
    status("writer", &dep_id, DeploymentState::InProgress, "")
        .await
        .unwrap();
    status(
        "writer",
        &dep_id,
        DeploymentState::Success,
        "https://prod.example",
    )
    .await
    .unwrap();
    let env = w
        .svc
        .get_deployment_environment(common::req(
            &w.token("reader"),
            GetDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "production".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(env.url, "https://prod.example");
    let statuses = w
        .svc
        .list_deployment_statuses(common::req(
            &w.token("reader"),
            ListDeploymentStatusesRequest {
                repo: w.rr(),
                deployment_id: dep_id.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .statuses;
    assert_eq!(
        statuses[0].state,
        DeploymentState::Success as i32,
        "newest first"
    );
    assert_eq!(statuses.len(), 4, "pending, queued, in_progress, success");
    let reviews = w
        .svc
        .list_deployment_reviews(common::req(
            &w.token("reader"),
            ListDeploymentReviewsRequest {
                repo: w.rr(),
                deployment_id: dep_id.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .reviews;
    assert_eq!(reviews[0].reviewer.as_ref().unwrap().id, "admin");
    assert_eq!(
        st(w.svc
            .delete_deployment(common::req(
                &w.token("writer"),
                DeleteDeploymentRequest {
                    repo: w.rr(),
                    deployment_id: dep_id.clone(),
                },
            ))
            .await)
        .0,
        "INVALID_STATE",
        "the active deployment cannot be deleted"
    );

    // A later successful deployment to an environment without rules supersedes the first.
    let first = create("writer", "staging", Some(vec![]))
        .await
        .unwrap()
        .into_inner();
    status("writer", &first.id, DeploymentState::Success, "")
        .await
        .unwrap();
    let second = create("writer", "staging", Some(vec![]))
        .await
        .unwrap()
        .into_inner();
    status("writer", &second.id, DeploymentState::Success, "")
        .await
        .unwrap();
    let staging = w
        .svc
        .list_deployments(common::req(
            &w.token("reader"),
            ListDeploymentsRequest {
                repo: w.rr(),
                environment: "staging".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .deployments;
    assert_eq!(staging[0].id, second.id, "newest first");
    assert_eq!(staging[1].state, DeploymentState::Inactive as i32);
    w.svc
        .delete_deployment(common::req(
            &w.token("writer"),
            DeleteDeploymentRequest {
                repo: w.rr(),
                deployment_id: first.id.clone(),
            },
        ))
        .await
        .unwrap();

    // Required contexts: every check reported on the changeset, unless listed explicitly.
    w.forge
        .checks
        .report(
            &w.cx("writer"),
            &w.org.id,
            &w.repo.id,
            Report {
                changeset: Some(c1.clone()),
                name: "lint".into(),
                status: CheckStatus::Failed,
                ..Report::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        st(create("writer", "staging", None).await).0,
        "POLICY_DENIED"
    );
    create("writer", "staging", Some(vec![])).await.unwrap();

    // Ref policies and green gating.
    w.svc
        .create_deployment_environment(common::req(
            &w.token("owner"),
            CreateDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "release".into(),
                ref_policy_mode: DeploymentRefPolicyMode::Custom as i32,
                ref_policies: vec![DeploymentRefPolicy {
                    pattern: "release/*".into(),
                    r#type: DeploymentRefPolicyType::Branch as i32,
                }],
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        st(create("writer", "release", Some(vec![])).await).0,
        "POLICY_DENIED"
    );
    w.svc
        .create_deployment_environment(common::req(
            &w.token("owner"),
            CreateDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "canary".into(),
                require_green: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        st(create("writer", "canary", Some(vec![])).await).0,
        "POLICY_DENIED",
        "no post-land check has passed yet"
    );
    w.forge
        .checks
        .report(
            &w.cx("writer"),
            &w.org.id,
            &w.repo.id,
            Report {
                changeset: Some(c1.clone()),
                name: "test".into(),
                status: CheckStatus::Passed,
                post_land: true,
                ..Report::default()
            },
        )
        .await
        .unwrap();
    let green = w
        .svc
        .get_deployment_green_status(common::req(
            &w.token("reader"),
            GetDeploymentGreenStatusRequest {
                repo: w.rr(),
                r#ref: "main".into(),
                changeset: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(green.green);
    assert_eq!(green.green_ref, "green/main");
    assert_eq!(green.green_tip, Some(c1.clone()));
    create("writer", "canary", Some(vec![])).await.unwrap();
    let c2 = w.push(&[("x.txt", Some("x"))], "more").await;
    let green =
        w.ci.green_status(&w.cx("owner"), &w.repo, "main", None)
            .await
            .unwrap();
    assert!(!green.green, "the new tip has not been tested");
    assert_eq!(green.tip, Some(c2));
    assert_eq!(
        green.green_tip,
        Some(c1),
        "green/main stays on the last green"
    );
    // Pre-deploy testing: the untested tip cannot reach a green-gated environment.
    assert_eq!(
        st(create("writer", "canary", Some(vec![])).await).0,
        "POLICY_DENIED",
        "a deploy of the untested tip is refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_protection_rules_gate_deployments_and_workflow_jobs() {
    let w = world().await;
    w.push(&[("README.md", Some("# app\n"))], "init").await;
    w.svc
        .create_deployment_environment(common::req(
            &w.token("owner"),
            CreateDeploymentEnvironmentRequest {
                repo: w.rr(),
                name: "production".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let rule = w
        .svc
        .create_deployment_protection_rule(common::req(
            &w.token("owner"),
            CreateDeploymentProtectionRuleRequest {
                repo: w.rr(),
                environment: "production".into(),
                app_id: "deploy-bot".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(rule.enabled);
    assert_eq!(
        st(w.svc
            .list_deployment_protection_rule_apps(common::req(
                &w.token("owner"),
                ListDeploymentProtectionRuleAppsRequest {
                    repo: w.rr(),
                    environment: "production".into(),
                    page: None,
                },
            ))
            .await)
        .1,
        "served_elsewhere"
    );
    // An API deployment waits for the app.
    assert_eq!(
        st(w.svc
            .create_deployment(common::req(
                &w.token("writer"),
                CreateDeploymentRequest {
                    repo: w.rr(),
                    r#ref: "main".into(),
                    environment: "production".into(),
                    required_contexts: Some(DeploymentRequiredContexts { contexts: vec![] }),
                    ..Default::default()
                },
            ))
            .await)
        .0,
        "APPROVAL_REQUIRED"
    );
    let pending =
        w.ci.pending_reviews(&w.cx("owner"), &w.repo, "production", "owner", true)
            .await
            .unwrap();
    assert_eq!(pending[0].pending_rule_app_slugs, vec!["deploy-bot"]);
    let report = |who: &str, id: &str, d: DeploymentReviewDecision| {
        w.svc.report_deployment_protection_rule_result(common::req(
            &w.token(who),
            ReportDeploymentProtectionRuleResultRequest {
                repo: w.rr(),
                deployment_id: id.into(),
                environment: "production".into(),
                decision: d as i32,
                comment: "checked".into(),
            },
        ))
    };
    let dep = pending[0].deployment_id.clone();
    assert_eq!(
        st(report("writer", &dep, DeploymentReviewDecision::Approved).await).0,
        "PERMISSION_DENIED"
    );
    report("deploy-bot", &dep, DeploymentReviewDecision::Approved)
        .await
        .unwrap();
    let d =
        w.ci.get_deployment(&w.cx("owner"), &w.repo, &dep)
            .await
            .unwrap();
    assert_eq!(d.state, DeploymentState::Queued as i32);
    assert_eq!(
        st(report("deploy-bot", &dep, DeploymentReviewDecision::Approved).await).0,
        "INVALID_STATE"
    );

    // A workflow job deploying to the environment waits for the app, then runs.
    let wf = "on: push\njobs:\n  ship:\n    runs-on: ubuntu-latest\n    environment: production\n    steps:\n      - run: echo shipping\n";
    w.push(&[(".github/workflows/ship.yml", Some(wf))], "ship")
        .await;
    let runner = w.runner("r", None).await;
    assert!(
        w.drain(&runner).await.is_empty(),
        "the job waits on the rule"
    );
    let run = w.runs().await.remove(0);
    let job = w.jobs(&run.id).await.remove(0);
    assert_eq!(job.status, WorkflowStatus::Waiting as i32);
    let gated =
        w.ci.pending_reviews(&w.cx("owner"), &w.repo, "production", "owner", true)
            .await
            .unwrap();
    let gated: Vec<_> = gated
        .into_iter()
        .filter(|p| p.workflow_run_id == run.id)
        .collect();
    assert_eq!(gated.len(), 1);
    report(
        "deploy-bot",
        &gated[0].deployment_id,
        DeploymentReviewDecision::Approved,
    )
    .await
    .unwrap();
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::Success));
    assert!(w.log(&job.id).await.contains("shipping"));
}

// ----- AS-CI-02: post-land bisect and automatic revert ---------------------------------------

const TEST_WF: &str = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: test \"$(cat value.txt)\" = good\n";

/// Facts from a matching merge-queue ruleset. `fail` models an unavailable ruleset lookup.
struct QueueFacts {
    fail: bool,
}

#[async_trait]
impl LandFacts for QueueFacts {
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
        if self.fail {
            Err(ForgeError::invalid(
                "ruleset_lookup_failed",
                "ruleset lookup failed",
            ))
        } else {
            Ok(target_ref == "main")
        }
    }
}

/// Drives runners, the event relay and the bisect tick until no bisect runs (bounded).
async fn settle(w: &World, runner: &LocalRunner) {
    for _ in 0..40 {
        w.drain(runner).await;
        w.forge.flush().await.unwrap();
        w.bisector.tick(&Cx::system()).await.unwrap();
        let running = w
            .bisector
            .list(&w.cx("owner"), &w.repo, BisectState::Running)
            .await
            .unwrap();
        if running.is_empty() && runner.run_once().await.unwrap().is_none() {
            w.forge.flush().await.unwrap();
            return;
        }
    }
    panic!("bisect did not settle");
}

/// Runs a minimal post-land regression through the bisector and returns its queued revert.
async fn queue_auto_revert(w: &World, facts: Arc<dyn LandFacts>) -> (ObjectId, ChangeRequest) {
    w.forge
        .orgs
        .update_repo_settings(&w.cx("owner"), &w.repo.id, 0, |s| {
            s.auto_revert = true;
            Ok(())
        })
        .await
        .unwrap();
    let base = w
        .push(
            &[
                ("value.txt", Some("good")),
                (".github/workflows/ci.yml", Some(TEST_WF)),
            ],
            "install ci",
        )
        .await;
    let runner = w.runner("r", None).await;
    settle(w, &runner).await;

    let head = w
        .commit_in(
            &w.repo,
            Some(&w.tip().await),
            &[("value.txt", Some("bad"))],
            "break ci",
            "writer",
        )
        .await;
    let culprit = w
        .changes
        .open(
            &w.cx("writer"),
            &w.repo,
            NewChange {
                head: Some(head),
                title: "Break CI".into(),
                ..NewChange::default()
            },
        )
        .await
        .unwrap();
    let (culprit, _) = w
        .changes
        .land(
            &w.cx("writer"),
            &w.repo,
            &culprit.id,
            LandMethod::Rebase,
            "",
            0,
        )
        .await
        .unwrap();
    let tip_before_revert = w.tip().await;
    w.changes.set_land_facts(facts);
    w.announce(&w.repo).await;
    settle(w, &runner).await;

    let bisects = w
        .bisector
        .list(&w.cx("owner"), &w.repo, BisectState::Unspecified)
        .await
        .unwrap();
    assert_eq!(bisects.len(), 1, "{bisects:?}");
    let bisect = &bisects[0];
    assert_eq!(bisect.state, BisectState::Found as i32, "{bisect:?}");
    assert_eq!(bisect.good, Some(base));
    assert_eq!(bisect.first_bad, culprit.landed_ref.and_then(|r| r.target));
    let revert = w
        .changes
        .get(&w.cx("owner"), &w.repo, &bisect.revert_change_request_id)
        .await
        .unwrap();
    (tip_before_revert, revert)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_revert_is_queued_when_a_ruleset_requires_the_queue() {
    let w = world().await;
    assert!(
        !w.forge
            .orgs
            .repo_settings(&w.cx("owner"), &w.repo.id)
            .await
            .unwrap()
            .merge_queue_required
    );

    let (tip_before_revert, revert) =
        queue_auto_revert(&w, Arc::new(QueueFacts { fail: false })).await;
    assert_eq!(revert.state, ChangeState::Open as i32);
    assert_eq!(
        revert.merge_queue_entry.as_ref().unwrap().state,
        MergeQueueState::Queued as i32
    );
    assert_eq!(
        w.tip().await,
        tip_before_revert,
        "revert did not land directly"
    );

    let (landed, _) = w
        .changes
        .process_queue(&Cx::system(), &w.repo, "main")
        .await
        .unwrap()
        .expect("queued revert is processed");
    assert_eq!(landed.id, revert.id);
    assert_eq!(landed.state, ChangeState::Landed as i32);
    assert_ne!(w.tip().await, tip_before_revert, "queue step lands revert");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_revert_is_queued_when_the_facts_lookup_fails() {
    let w = world().await;

    let (tip_before_revert, revert) =
        queue_auto_revert(&w, Arc::new(QueueFacts { fail: true })).await;
    assert_eq!(revert.state, ChangeState::Open as i32);
    assert_eq!(
        revert.merge_queue_entry.as_ref().unwrap().state,
        MergeQueueState::Queued as i32
    );
    assert_eq!(
        w.tip().await,
        tip_before_revert,
        "failed lookup fails closed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_ci_02_regression_on_main_is_bisected_and_reverted() {
    let w = world().await;
    w.forge
        .orgs
        .update_repo_settings(&w.cx("owner"), &w.repo.id, 0, |s| {
            s.auto_revert = true;
            Ok(())
        })
        .await
        .unwrap();
    let base = w
        .push(
            &[
                ("value.txt", Some("good")),
                (".github/workflows/ci.yml", Some(TEST_WF)),
            ],
            "install ci",
        )
        .await;
    let runner = w.runner("r", None).await;
    settle(&w, &runner).await;
    assert!(
        w.forge
            .checks
            .failing_post_land(&w.cx("owner"), &w.repo.id, &hex(&base.raw))
            .await
            .unwrap()
            .is_empty()
    );

    // Eight agents' changes land back to back; the sixth breaks the test.
    let mut crs = Vec::new();
    for i in 1..=8 {
        let who = if i % 2 == 0 { "agent" } else { "writer" };
        let tip = w.tip().await;
        let file = format!("f{i}.txt");
        let mut edits: Vec<(&str, Option<&str>)> = vec![(file.as_str(), Some("x"))];
        if i == 6 {
            edits.push(("value.txt", Some("bad")));
        }
        let head = w
            .commit_in(&w.repo, Some(&tip), &edits, &format!("change {i}"), who)
            .await;
        let cr = w
            .changes
            .open(
                &w.cx(who),
                &w.repo,
                NewChange {
                    head: Some(head),
                    title: format!("Change {i}"),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap();
        let (landed, _) = w
            .changes
            .land(&w.cx(who), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
            .await
            .unwrap();
        crs.push(landed);
    }
    let culprit_cr = crs[5].clone();
    let culprit = culprit_cr.landed_ref.clone().unwrap().target.unwrap();
    // The post-land run covers the tip of the train.
    w.announce(&w.repo).await;
    settle(&w, &runner).await;

    let all = w
        .bisector
        .list(&w.cx("owner"), &w.repo, BisectState::Unspecified)
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    let b = &all[0];
    assert_eq!(b.state, BisectState::Found as i32, "{b:?}");
    assert_eq!(b.first_bad, Some(culprit.clone()));
    assert_eq!(b.good, Some(base.clone()));
    assert_eq!(b.check_name, "test");
    let session = w
        .bisector
        .session(&w.cx("owner"), &w.repo, &b.id)
        .await
        .unwrap();
    assert_eq!(session.candidates.len(), 8);
    assert!(session.rounds <= 4, "{} rounds", session.rounds);
    assert!(
        session.probes.iter().all(|p| p.outcome.is_some()),
        "no probe left running: {:?}",
        session.probes
    );
    assert!(!b.tested.is_empty());
    assert_eq!(session.culprit_change_request_id, culprit_cr.id);
    assert_eq!(session.culprit_author, "agent");
    assert!(session.revert_landed, "{}", session.note);

    // The revert CR: opened through the changes domain, linked to the failing run, landed.
    let revert = w
        .changes
        .get(&w.cx("owner"), &w.repo, &b.revert_change_request_id)
        .await
        .unwrap();
    assert_eq!(revert.revert_of, culprit_cr.id);
    assert_eq!(revert.state, ChangeState::Landed as i32);
    assert!(
        revert
            .body
            .contains(&format!("Reverts #{}", culprit_cr.number))
    );
    assert!(revert.body.contains("/actions/runs/"), "{}", revert.body);
    assert!(
        revert
            .review_requests
            .iter()
            .any(|r| r.reviewer.as_ref().is_some_and(|p| p.id == "agent")),
        "the culprit's author is asked to review the revert"
    );
    let tip = w.tip().await;
    let value = w.vcs.read_file(&w.repo, &tip, "value.txt").await.unwrap();
    assert_eq!(value.as_deref(), Some(&b"good"[..]));
    let f8 = w.vcs.read_file(&w.repo, &tip, "f8.txt").await.unwrap();
    assert!(f8.is_some(), "later changes stay");

    // The culprit's author hears about it.
    let events = w.kinds().await;
    let found = events
        .iter()
        .find(|e| e.kind == "bisect.found")
        .expect("bisect.found");
    let subject = found.subject.clone().unwrap();
    assert_eq!(subject.kind, SubjectKind::ChangeRequest as i32);
    assert_eq!(subject.id, culprit_cr.number.to_string());
    assert!(found.payload_json.contains("\"culprit_author\":\"agent\""));
    assert!(events.iter().any(|e| e.kind == "bisect.reverted"));
    let (comments, _) = w
        .changes
        .comments(&w.cx("owner"), &w.repo, &culprit_cr.id, "", 100)
        .await
        .unwrap();
    assert!(
        comments.iter().any(|c| c.body.contains("@agent")),
        "{comments:?}"
    );

    // The revert's own post-land run goes green; nothing is bisected again.
    w.announce(&w.repo).await;
    settle(&w, &runner).await;
    let green =
        w.ci.green_status(&w.cx("owner"), &w.repo, "main", None)
            .await
            .unwrap();
    assert!(green.green, "{green:?}");
    assert!(!green.bisect_in_progress);
    assert_eq!(
        w.bisector
            .list(&w.cx("owner"), &w.repo, BisectState::Unspecified)
            .await
            .unwrap()
            .len(),
        1
    );
    // Probe runs reported no check runs on the candidates.
    for c in &session.candidates[..7] {
        let checks = w
            .forge
            .checks
            .check_results(&w.cx("owner"), &w.repo.id, &hex(&c.raw))
            .await
            .unwrap();
        assert!(
            checks.is_empty(),
            "probe reported a check on {}",
            hex(&c.raw)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_bisect_sessions_follow_good_and_bad_marks() {
    let w = world().await;
    let mut chain = vec![w.push(&[("value.txt", Some("good"))], "base").await];
    for i in 1..=6 {
        let file = format!("f{i}");
        let mut edits = vec![(file.as_str(), Some("x"))];
        if i == 3 {
            edits.push(("value.txt", Some("bad")));
        }
        let c = w.push(&edits, &format!("c{i}")).await;
        chain.push(c);
    }
    let svc = BisectCheckService::new(w.forge.clone(), w.bisector.clone());
    let start = |who: &str, good: Option<ObjectId>| {
        svc.start_bisect(common::req(
            &w.token(who),
            StartBisectRequest {
                repo: w.rr(),
                r#ref: "main".into(),
                check_name: "manual-check".into(),
                good,
                bad: None,
                open_revert: false,
                manual: false,
            },
        ))
    };
    assert_eq!(
        st(start("reader", Some(chain[0].clone())).await).0,
        "PERMISSION_DENIED"
    );
    assert_eq!(
        st(start("writer", None).await),
        ("INVALID_ARGUMENT".into(), "no_known_good".into()),
        "nothing reports the check, so the good end must be given"
    );
    let b = start("writer", Some(chain[0].clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(b.state, BisectState::Running as i32);
    assert_eq!(
        st(start("writer", Some(chain[0].clone())).await).0,
        "INVALID_STATE",
        "one session per ref and check"
    );
    let session = w
        .bisector
        .session(&w.cx("writer"), &w.repo, &b.id)
        .await
        .unwrap();
    assert_eq!(session.mode, "manual");
    assert_eq!(session.candidates.len(), 6);
    assert_eq!(
        w.bisector
            .mark(&w.cx("writer"), &w.repo, &b.id, &chain[0], Outcome::Pass)
            .await
            .unwrap_err()
            .subcode,
        "not_a_candidate"
    );
    let mut marks = 0;
    while let Some(next) = w
        .bisector
        .next_candidate(&w.cx("writer"), &w.repo, &b.id)
        .await
        .unwrap()
    {
        let value = w.vcs.read_file(&w.repo, &next, "value.txt").await.unwrap();
        let outcome = if value.as_deref() == Some(&b"good"[..]) {
            Outcome::Pass
        } else {
            Outcome::Fail
        };
        w.bisector
            .mark(&w.cx("writer"), &w.repo, &b.id, &next, outcome)
            .await
            .unwrap();
        marks += 1;
        assert!(
            marks <= 3,
            "binary search over six candidates needs three marks"
        );
    }
    let got = svc
        .get_bisect(common::req(
            &w.token("reader"),
            GetBisectRequest {
                repo: w.rr(),
                id: b.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.state, BisectState::Found as i32);
    assert_eq!(got.first_bad, Some(chain[3].clone()));
    assert!(got.revert_change_request_id.is_empty());
    let session = w
        .bisector
        .session(&w.cx("writer"), &w.repo, &b.id)
        .await
        .unwrap();
    assert!(
        session.note.contains("not landed through a change request"),
        "{}",
        session.note
    );
    let listed = svc
        .list_bisects(common::req(
            &w.token("reader"),
            ListBisectsRequest {
                repo: w.rr(),
                state: BisectState::Found as i32,
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .bisects;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        st(svc
            .list_bisects(common::req(
                &w.token("outsider"),
                ListBisectsRequest {
                    repo: w.rr(),
                    state: 0,
                    page: None,
                },
            ))
            .await)
        .0,
        "NOT_FOUND",
        "a private repo is invisible to outsiders"
    );
    // A cancelled session ends without a culprit.
    let again = w
        .bisector
        .start(
            &w.cx("writer"),
            &w.repo,
            StartBisect {
                check_name: "manual-check".into(),
                good: Some(chain[0].clone()),
                manual: true,
                ..StartBisect::default()
            },
        )
        .await
        .unwrap();
    let cancelled = w
        .bisector
        .cancel(&w.cx("writer"), &w.repo, &again.id)
        .await
        .unwrap();
    assert_eq!(cancelled.state, BisectState::Cancelled as i32);
}

// ----- the action registry -------------------------------------------------------------------

fn node_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("node").is_file()))
}

const JS_ACTION: &str = "name: hello-js\ninputs:\n  who:\n    default: world\n  greeting style:\n    default: plain\noutputs:\n  greeting:\n    description: the greeting\nruns:\n  using: node20\n  pre: pre.js\n  main: main.js\n  post: post.js\n";
const JS_PRE: &str = "const fs = require('fs');\nconsole.log('pre for ' + process.env.INPUT_WHO);\nfs.appendFileSync(process.env.GITHUB_STATE, 'started=yes\\n');\n";
const JS_MAIN: &str = "const fs = require('fs');\nconst who = process.env.INPUT_WHO;\nconsole.log('main says hello ' + who + ' (' + process.env['INPUT_GREETING_STYLE'] + ') state=' + process.env.STATE_started);\nfs.appendFileSync(process.env.GITHUB_OUTPUT, 'greeting=hello ' + who + '\\n');\nfs.appendFileSync(process.env.GITHUB_ENV, 'GREETED=' + who + '\\n');\nfs.appendFileSync(process.env.GITHUB_STATE, 'cleanup=' + who + '\\n');\nfs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, '### greeted ' + who + '\\n');\nconsole.log('action path ' + (process.env.GITHUB_ACTION_PATH.endsWith('hello-js') ? 'ok' : process.env.GITHUB_ACTION_PATH));\n";
const LOCAL_JS: &str = "name: local-js\nruns:\n  using: node20\n  pre: pre.js\n  main: main.js\n";
const LOCAL_PRE: &str = "require('fs').appendFileSync(process.env.GITHUB_STATE, 'seen=1\\n');\nconsole.log('local pre ran');\n";
const LOCAL_MAIN: &str = "console.log('local main sees seen=' + process.env.STATE_seen);\n";
const JS_POST: &str = "console.log('post cleans up ' + process.env.STATE_cleanup + ' started=' + process.env.STATE_started);\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn javascript_action_runs_with_node_inputs_state_and_post() {
    if !node_on_path() {
        eprintln!(
            "skipping javascript_action_runs_with_node_inputs_state_and_post: `node` is not on PATH"
        );
        return;
    }
    let w = world().await;
    let tools = w.new_repo(&w.org, "tools", Visibility::Private).await;
    let c = w
        .commit_in(
            &tools,
            None,
            &[
                ("hello-js/action.yml", Some(JS_ACTION)),
                ("hello-js/pre.js", Some(JS_PRE)),
                ("hello-js/main.js", Some(JS_MAIN)),
                ("hello-js/post.js", Some(JS_POST)),
            ],
            "js action",
            "owner",
        )
        .await;
    w.vcs.set_ref(&tools, "main", &c).await.unwrap();
    let wf = format!(
        "on: push\njobs:\n  js:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - id: h\n        uses: {}/tools/hello-js@main\n        with:\n          who: bee\n      - run: echo \"out=${{{{ steps.h.outputs.greeting }}}} env=$GREETED\"\n      - uses: ./local-js\n",
        w.org.name
    );
    w.push(
        &[
            (".github/workflows/js.yml", Some(wf.as_str())),
            ("local-js/action.yml", Some(LOCAL_JS)),
            ("local-js/pre.js", Some(LOCAL_PRE)),
            ("local-js/main.js", Some(LOCAL_MAIN)),
        ],
        "js",
    )
    .await;
    let registry = ActionRegistry::new(Arc::new(ForgeActionSource::new(w.ci.clone())));
    let runner = w.runner("r", Some(registry)).await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 1);
    let job = w.jobs(&w.runs().await[0].id).await.remove(0);
    let log = w.log(&job.id).await;
    assert_eq!(
        ran[0].conclusion,
        Some(WorkflowConclusion::Success),
        "{log}"
    );
    let pre = log.find("pre for bee").expect(&log);
    let main = log
        .find("main says hello bee (plain) state=yes")
        .expect(&log);
    let post = log.find("post cleans up bee started=yes").expect(&log);
    assert!(pre < main && main < post, "{log}");
    assert!(log.contains("out=hello bee env=bee"), "{log}");
    assert!(log.contains("action path ok"), "{log}");
    let local_pre = log.find("local pre ran").expect(&log);
    let local_main = log.find("local main sees seen=1").expect(&log);
    assert!(
        main < local_pre && local_pre < local_main && local_main < post,
        "{log}"
    );
    let summary = w
        .svc
        .get_workflow_job_summary(common::req(
            &w.token("owner"),
            GetWorkflowJobSummaryRequest {
                repo: w.rr(),
                job_id: job.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(format!("{summary:?}").contains("greeted bee"));
}

const COMPOSITE: &str = "name: greet\ninputs:\n  who:\n    required: true\noutputs:\n  said:\n    value: ${{ steps.s.outputs.said }}\nruns:\n  using: composite\n  steps:\n    - id: s\n      shell: bash\n      run: bash ${{ github.action_path }}/greet.sh \"${{ inputs.who }}\"\n";
const GREET_SH: &str = "#!/usr/bin/env bash\necho \"said=hi $1\" >> \"$GITHUB_OUTPUT\"\necho \"greet.sh greets $1 from $GITHUB_ACTION_REPOSITORY\"\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composite_actions_from_fetched_repos_and_mirrors_docker_still_rejected() {
    let w = world().await;
    let tools = w.new_repo(&w.org, "tools", Visibility::Private).await;
    let c = w
        .commit_in(
            &tools,
            None,
            &[
                ("greet/action.yml", Some(COMPOSITE)),
                (
                    "dock/action.yml",
                    Some("name: dock\nruns:\n  using: docker\n  image: Dockerfile\n"),
                ),
            ],
            "actions",
            "owner",
        )
        .await;
    let c = w
        .vcs
        .create_commit(
            &tools,
            NewCommit {
                parent: Some(c),
                edits: vec![("greet/greet.sh".into(), Some(GREET_SH.as_bytes().to_vec()))],
                message: "script".into(),
                author: "owner".into(),
                creator: "owner".into(),
                chain: vec!["owner".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    w.vcs.set_ref(&tools, "main", &c).await.unwrap();
    // A mirror org holding an action imported from github.com/someone/fancy, tagged v1.
    let cx = w.cx("owner");
    let mirror_name = format!("m{}", w.forge.db.ids().ulid().unwrap().to_lowercase());
    let mirror_org = w
        .forge
        .orgs
        .create_org(
            &cx,
            &mirror_name,
            "",
            w.directory.get("owner").await.unwrap(),
        )
        .await
        .unwrap();
    w.forge
        .hooks
        .provisioner
        .create_org(&mirror_org)
        .await
        .unwrap();
    let fancy = w.new_repo(&mirror_org, "fancy", Visibility::Public).await;
    let fc = w
        .commit_in(
            &fancy,
            None,
            &[(
                "action.yml",
                Some("name: fancy\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo fancy mirror action ran at ${{ github.action_ref }}\n"),
            )],
            "fancy",
            "owner",
        )
        .await;
    w.vcs.set_ref(&fancy, "tags/v1", &fc).await.unwrap();
    let wf = format!(
        "on: push\njobs:\n  comp:\n    runs-on: ubuntu-latest\n    steps:\n      - id: g\n        uses: {org}/tools/greet@main\n        with:\n          who: agents\n      - run: echo \"result ${{{{ steps.g.outputs.said }}}}\"\n      - uses: someone/fancy@v1\n  docker:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: {org}/tools/dock@main\n  missing:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: {org}/tools/greet@main\n",
        org = w.org.name
    );
    w.push(&[(".github/workflows/c.yml", Some(wf.as_str()))], "c")
        .await;
    let registry = ActionRegistry::new(Arc::new(
        ForgeActionSource::new(w.ci.clone()).with_mirror("someone", &mirror_name),
    ));
    let runner = w.runner("r", Some(registry)).await;
    w.drain(&runner).await;
    let jobs = w.jobs(&w.runs().await[0].id).await;
    let by = |n: &str| jobs.iter().find(|j| j.name == n).unwrap().clone();
    let log = w.log(&by("comp").id).await;
    assert_eq!(
        by("comp").conclusion,
        WorkflowConclusion::Success as i32,
        "{log}"
    );
    assert!(
        log.contains(&format!("greet.sh greets agents from {}/tools", w.org.name)),
        "{log}"
    );
    assert!(log.contains("result hi agents"), "{log}");
    assert!(log.contains("fancy mirror action ran at v1"), "{log}");
    let dlog = w.log(&by("docker").id).await;
    assert_eq!(by("docker").conclusion, WorkflowConclusion::Failure as i32);
    assert!(
        dlog.contains("Docker actions are not supported by the local runner"),
        "{dlog}"
    );
    let mlog = w.log(&by("missing").id).await;
    assert_eq!(by("missing").conclusion, WorkflowConclusion::Failure as i32);
    assert!(mlog.contains("input who is required"), "{mlog}");
}

// ----- pull_request_review triggers ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_request_review_events_trigger_workflows() {
    let w = world().await;
    let wf = "on:\n  pull_request_review:\n    types: [submitted]\njobs:\n  r:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo review ${{ github.event.review.state }} on ${{ github.event.pull_request.number }}\n";
    w.push(&[(".github/workflows/review.yml", Some(wf))], "install")
        .await;
    let tip = w.tip().await;
    let head = w
        .commit_in(
            &w.repo,
            Some(&tip),
            &[("x.txt", Some("x"))],
            "change",
            "agent",
        )
        .await;
    let cr = w
        .changes
        .open(
            &w.cx("agent"),
            &w.repo,
            NewChange {
                head: Some(head),
                title: "a change".into(),
                ..NewChange::default()
            },
        )
        .await
        .unwrap();
    w.forge.flush().await.unwrap();
    assert!(
        w.runs()
            .await
            .iter()
            .all(|r| r.event != "pull_request_review"),
        "opening is not a review"
    );
    w.changes
        .submit_review(
            &w.cx("writer"),
            &w.repo,
            &cr.id,
            0,
            Verdict::Approve,
            "lgtm",
            vec![],
        )
        .await
        .unwrap();
    w.forge.flush().await.unwrap();
    let runs: Vec<WorkflowRun> = w
        .runs()
        .await
        .into_iter()
        .filter(|r| r.event == "pull_request_review")
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].change_request_numbers, vec![cr.number]);
    let runner = w.runner("r", None).await;
    w.drain(&runner).await;
    let job = w.jobs(&runs[0].id).await.remove(0);
    let log = w.log(&job.id).await;
    assert!(
        log.contains(&format!("review approved on {}", cr.number)),
        "{log}"
    );
}

/// The scenario's future is larger than the default 2 MiB test thread stack in a debug build,
/// so it runs on its own thread with 16 MiB.
#[test]
fn fork_jobs_get_read_tokens_unless_the_policy_sends_write_tokens() {
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(fork_job_token_scenario())
        })
        .expect("spawn the scenario thread")
        .join()
        .expect("the scenario panicked");
}

async fn fork_job_token_scenario() {
    let w = world().await;
    let workflow = "on: pull_request\npermissions:\n  contents: write\n  issues: write\njobs:\n  inherited:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo inherited\n  write_all:\n    runs-on: ubuntu-latest\n    permissions: write-all\n    steps:\n      - run: echo override\n";
    let fallback_workflow = "on: pull_request\njobs:\n  default:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo default\n";
    w.push(
        &[
            (".github/workflows/permissions.yml", Some(workflow)),
            (
                ".github/workflows/default-permissions.yml",
                Some(fallback_workflow),
            ),
        ],
        "install token permission workflows",
    )
    .await;
    let fork = w.new_repo(&w.org, "fork", Visibility::Private).await;
    let policy_scope = PolicyScope::Repo(Box::new(w.repo.clone()));
    let set_policy = |fork_send_write_tokens| SetActionsPolicyRequest {
        fork_send_write_tokens: Some(fork_send_write_tokens),
        default_token_permission: Some(ActionsPolicyTokenPermission::Write as i32),
        ..Default::default()
    };

    async fn open_cr(w: &World, head_repo_id: &str, marker: &str) -> ChangeRequest {
        let head = w
            .commit_in(
                &w.repo,
                Some(&w.tip().await),
                &[("permission-marker.txt", Some(marker))],
                marker,
                "writer",
            )
            .await;
        w.changes
            .open(
                &w.cx("writer"),
                &w.repo,
                NewChange {
                    head: Some(head),
                    title: marker.into(),
                    head_repo_id: head_repo_id.into(),
                    head_ref: "feature".into(),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap()
    }

    async fn run_jobs(w: &World, cr: &ChangeRequest, workflow: &str) -> Vec<JobRecord> {
        w.forge.flush().await.unwrap();
        let run = w
            .runs()
            .await
            .into_iter()
            .find(|run| {
                run.change_request_numbers == [cr.number] && run.workflow_path.ends_with(workflow)
            })
            .unwrap_or_else(|| panic!("no run for change request {}", cr.number));
        if run.status == WorkflowStatus::ActionRequired as i32 {
            w.ci.approve_run(&w.cx("owner"), &w.repo.id, &run.id)
                .await
                .unwrap();
            w.forge.flush().await.unwrap();
        }
        let mut tx = w
            .forge
            .db
            .begin(bee_forge::ci::SURFACE, &w.cx("owner"))
            .await
            .unwrap();
        let jobs = tx
            .scan_msgs::<JobRecord>(
                &bee_forge::ci::JOBS,
                &Scan::scope(w.repo.id.clone(), 5000).key(Key::K1(run.id.clone())),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|(job, _)| job)
            .collect::<Vec<_>>();
        tx.commit().await.unwrap();
        assert!(!jobs.is_empty(), "workflow did not create jobs: {run:?}");
        jobs
    }

    fn job_name(job: &JobRecord) -> &str {
        &job.job.as_ref().unwrap().name
    }

    w.ci.set_policy(&w.cx("owner"), &policy_scope, &set_policy(false))
        .await
        .unwrap();
    let fork_cr = open_cr(&w, &fork.id, "fork-denied").await;
    let mut fork_jobs = run_jobs(&w, &fork_cr, ".github/workflows/permissions.yml").await;
    fork_jobs.extend(run_jobs(&w, &fork_cr, ".github/workflows/default-permissions.yml").await);
    for job in &fork_jobs {
        assert!(
            job.permissions
                .iter()
                .all(|permission| !permission.ends_with(":write")),
            "fork write permission escaped the policy cap: {job:?}"
        );
        for scope in ["contents", "issues"] {
            assert!(
                job.permissions
                    .iter()
                    .any(|permission| permission == &format!("{scope}:read")),
                "expected capped {scope}:read on {job:?}"
            );
        }
    }

    let inherited = fork_jobs
        .iter()
        .find(|job| job_name(job).contains("inherited"))
        .unwrap();
    assert!(
        !inherited
            .permissions
            .iter()
            .any(|permission| permission == "actions:read"),
        "an omitted workflow permission was upgraded from none: {inherited:?}"
    );

    let fallback = fork_jobs
        .iter()
        .find(|job| job_name(job).contains("default"))
        .unwrap();
    assert!(fallback.permissions.contains(&"contents:read".into()));
    assert!(fallback.permissions.contains(&"packages:read".into()));
    assert!(!fallback.permissions.iter().any(|p| p.ends_with(":write")));

    w.ci.set_policy(&w.cx("owner"), &policy_scope, &set_policy(true))
        .await
        .unwrap();
    let allowed_cr = open_cr(&w, &fork.id, "fork-allowed").await;
    let allowed_jobs = run_jobs(&w, &allowed_cr, ".github/workflows/permissions.yml").await;
    let inherited = allowed_jobs
        .iter()
        .find(|job| job_name(job).contains("inherited"))
        .unwrap();
    assert!(inherited.permissions.contains(&"contents:write".into()));
    assert!(inherited.permissions.contains(&"issues:write".into()));
    let write_all = allowed_jobs
        .iter()
        .find(|job| job_name(job).contains("write_all"))
        .unwrap();
    assert!(write_all.permissions.contains(&"contents:write".into()));
    assert!(write_all.permissions.contains(&"issues:write".into()));

    w.ci.set_policy(&w.cx("owner"), &policy_scope, &set_policy(false))
        .await
        .unwrap();
    let local_cr = open_cr(&w, "", "local-run").await;
    let local_jobs = run_jobs(&w, &local_cr, ".github/workflows/permissions.yml").await;
    let local_inherited = local_jobs
        .iter()
        .find(|job| job_name(job).contains("inherited"))
        .unwrap();
    assert!(
        local_inherited
            .permissions
            .contains(&"contents:write".into())
    );
    assert!(local_inherited.permissions.contains(&"issues:write".into()));
}

// ----- signed transfer URLs ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_urls_serve_secrets_once_logs_and_artifacts() {
    let w = world().await;
    w.ci.put_secret(
        &w.cx("owner"),
        &VaultScope::Repo(w.repo.id.clone(), w.org.id.clone()),
        "TOK",
        b"s3cret-value",
        SecretVisibility::Unspecified,
        vec![],
    )
    .await
    .unwrap();
    let wf = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo transfer-log-line && echo data > a.txt\n      - uses: actions/upload-artifact@v4\n        with:\n          name: out\n          path: a.txt\n";
    w.push(
        &[
            (".github/workflows/one.yml", Some(wf)),
            (".github/workflows/two.yml", Some(wf)),
        ],
        "two workflows",
    )
    .await;
    // One job is claimed by hand to redeem its one-time secrets URL.
    let cx = w.cx("owner");
    let owner = RunnerOwner::Repo(w.repo.id.clone(), w.org.id.clone());
    let (token, _) = w.ci.runner_token(&cx, &owner, "register").await.unwrap();
    let (manual, _) =
        w.ci.register_runner(
            &Cx::system(),
            &token,
            "manual",
            RunnerOs::Linux,
            RunnerArch::X64,
            &["ubuntu-latest".to_string()],
            "",
            false,
            "test",
        )
        .await
        .unwrap();
    let a =
        w.ci.claim(&Cx::system(), &manual.id, &[], 1)
            .await
            .unwrap()
            .expect("a queued job");
    let secrets =
        w.ci.serve_transfer("GET", &a.secrets_url, Vec::new())
            .await
            .unwrap();
    assert_eq!(secrets.content_type, "application/json");
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&secrets.body).unwrap();
    assert_eq!(map["TOK"], "s3cret-value");
    let again =
        w.ci.serve_transfer("GET", &a.secrets_url, Vec::new())
            .await
            .unwrap_err();
    assert_eq!(
        again.reason,
        ErrorReason::PermissionDenied,
        "secrets redeem once"
    );
    // The other job runs, logs and uploads its artifact through the same URLs.
    let runner = w.runner("r", None).await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::Success));
    let job_id = ran[0].job_id.clone();
    let logs = w
        .svc
        .get_workflow_job_logs_url(common::req(
            &w.token("reader"),
            GetWorkflowJobLogsUrlRequest {
                repo: w.rr(),
                job_id: job_id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .url;
    let got = w.ci.serve_transfer("GET", &logs, Vec::new()).await.unwrap();
    assert!(String::from_utf8_lossy(&got.body).contains("transfer-log-line"));
    assert_eq!(
        w.ci.serve_transfer("PUT", &logs, b"x".to_vec())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied,
        "a log URL does not accept uploads"
    );
    let tampered = logs.replace("/logs?", "/logz?");
    assert_eq!(
        w.ci.serve_transfer("GET", &tampered, Vec::new())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        w.ci.serve_transfer("GET", "/_ci/repos/x/jobs/y/logs", Vec::new())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied,
        "unsigned"
    );
    let run_id = w
        .runs()
        .await
        .into_iter()
        .find(|r| r.status == WorkflowStatus::Completed as i32)
        .expect("the drained run")
        .id;
    let zip = w
        .svc
        .get_workflow_run_logs_url(common::req(
            &w.token("reader"),
            GetWorkflowRunLogsUrlRequest {
                repo: w.rr(),
                run_id: run_id.clone(),
                attempt: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .url;
    let z = w.ci.serve_transfer("GET", &zip, Vec::new()).await.unwrap();
    assert_eq!(z.content_type, "application/zip");
    assert!(z.body.starts_with(b"PK"));
    let arts = w
        .svc
        .list_workflow_artifacts(common::req(
            &w.token("reader"),
            ListWorkflowArtifactsRequest {
                repo: w.rr(),
                run_id: run_id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .artifacts;
    let url = w
        .svc
        .get_workflow_artifact_download_url(common::req(
            &w.token("reader"),
            GetWorkflowArtifactDownloadUrlRequest {
                repo: w.rr(),
                artifact_id: arts[0].id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .url;
    let art = w.ci.serve_transfer("GET", &url, Vec::new()).await.unwrap();
    assert!(
        art.body.starts_with(b"PK"),
        "the artifact is the runner's zip"
    );
}
