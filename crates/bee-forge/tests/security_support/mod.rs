//! Shared fixture of the security tests: a forge over a real in-memory VCS with the changes
//! and security modules attached, principals of every role, and the project files the tests
//! plant. Included by the other `security_*` test files as `mod security_support;`.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_auth::{InMemoryRevocations, KeyRing, MintRequest, Scope, SigningKey};
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, HistoryVcs, MemoryTeams};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PrincipalDirectory};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::security::ports::{
    AddedBlob, AdvisoryForks, MemoryFeed, MemoryRegistry, Ports, SecretVerifier, SecurityVcs,
    TreeFile,
};
use bee_forge::security::vcs::HistorySecurityVcs;
use bee_forge::security::{Security, SecurityServices};
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_policy::Action;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

pub const PACKAGE_JSON: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "dependencies": {
    "lodash": "^4.17.20"
  }
}
"#;

pub const PACKAGE_LOCK: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "app",
      "version": "1.0.0",
      "dependencies": {
        "lodash": "^4.17.20"
      }
    },
    "node_modules/lodash": {
      "version": "4.17.20",
      "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz",
      "integrity": "sha512-oldlodash"
    }
  }
}
"#;

pub const CARGO_TOML: &str = "[package]\nname = \"rustapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ntime = \"0.3.1\"\n";

pub const CARGO_LOCK: &str = "version = 3\n\n[[package]]\nname = \"rustapp\"\nversion = \"0.1.0\"\ndependencies = [\n \"time\",\n]\n\n[[package]]\nname = \"time\"\nversion = \"0.3.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aaaa\"\n";

pub const REQUIREMENTS: &str = "flask==2.0.1\nrequests==2.31.0\n";

pub const GO_MOD: &str = "module example.com/svc\n\ngo 1.21\n\nrequire example.com/m v1.0.0\n";

/// An OSV record for one package with a SEMVER or ECOSYSTEM range.
#[allow(clippy::too_many_arguments)]
pub fn osv(
    id: &str,
    cve: &str,
    ecosystem: &str,
    name: &str,
    range_type: &str,
    introduced: &str,
    fixed: &str,
    severity: &str,
) -> String {
    serde_json::json!({
        "id": id,
        "aliases": [cve],
        "summary": format!("{name} is vulnerable"),
        "details": format!("Details of {id}"),
        "modified": "2024-02-01T00:00:00Z",
        "published": "2024-01-01T00:00:00Z",
        "database_specific": {"severity": severity, "cwe_ids": ["CWE-79"]},
        "affected": [{
            "package": {"ecosystem": ecosystem, "name": name},
            "ranges": [{"type": range_type, "events": [{"introduced": introduced}, {"fixed": fixed}]}]
        }],
        "references": [{"type": "ADVISORY", "url": format!("https://example.test/{id}")}]
    })
    .to_string()
}

/// The four advisories that match the planted project files.
pub fn advisories() -> Vec<String> {
    vec![
        osv(
            "GHSA-npm1-aaaa-bbbb",
            "CVE-2099-0001",
            "npm",
            "lodash",
            "SEMVER",
            "0",
            "4.17.21",
            "CRITICAL",
        ),
        osv(
            "GHSA-crat-aaaa-bbbb",
            "CVE-2099-0002",
            "crates.io",
            "time",
            "SEMVER",
            "0.3.0",
            "0.3.36",
            "HIGH",
        ),
        osv(
            "GHSA-pypi-aaaa-bbbb",
            "CVE-2099-0003",
            "PyPI",
            "Flask",
            "ECOSYSTEM",
            "0",
            "2.0.2",
            "MODERATE",
        ),
        osv(
            "GHSA-gomo-aaaa-bbbb",
            "CVE-2099-0004",
            "Go",
            "example.com/m",
            "SEMVER",
            "0",
            "1.0.1",
            "LOW",
        ),
    ]
}

/// A verifier whose answers tests set per secret value.
#[derive(Default)]
pub struct TestVerifier {
    answers: RwLock<BTreeMap<String, SecretScanningValidity>>,
}

impl TestVerifier {
    pub fn set(&self, secret: &str, v: SecretScanningValidity) {
        self.answers.write().unwrap().insert(secret.to_string(), v);
    }
}

#[async_trait]
impl SecretVerifier for TestVerifier {
    async fn verify(&self, _secret_type: &str, secret: &[u8]) -> SecretScanningValidity {
        self.answers
            .read()
            .unwrap()
            .get(&String::from_utf8_lossy(secret).into_owned())
            .copied()
            .unwrap_or(SecretScanningValidity::Unknown)
    }
}

/// Creates private forks as plain repos named after the advisory.
pub struct TestForks {
    pub forge: Arc<Forge>,
    pub org_id: String,
}

#[async_trait]
impl AdvisoryForks for TestForks {
    async fn create_private_fork(&self, repo: &Repo, ghsa_id: &str) -> bee_forge::Result<Repo> {
        let _ = (repo, ghsa_id);
        self.forge
            .orgs
            .create_repo(
                &Cx::system(),
                &self.org_id,
                NewRepo {
                    id: self.forge.db.ids().ulid()?,
                    name: format!(
                        "advisory-fork-{}",
                        self.forge.db.ids().ulid()?.to_lowercase()
                    ),
                    ..NewRepo::default()
                },
            )
            .await
    }
}

pub const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("maintainer", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("secmgr", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("reviewer", PrincipalKind::Human),
    ("ci", PrincipalKind::Service),
    ("agent", PrincipalKind::Agent),
    ("bee-update-bot", PrincipalKind::Agent),
];

pub struct World {
    pub forge: Arc<Forge>,
    pub changes: Arc<Changes>,
    pub vcs: Arc<HistoryVcs>,
    pub sec: Arc<Security>,
    pub svc: SecurityServices,
    pub feed: Arc<MemoryFeed>,
    pub registry: Arc<MemoryRegistry>,
    pub verifier: Arc<TestVerifier>,
    pub directory: Arc<MemoryDirectory>,
    pub ring: KeyRing,
    pub clock: Arc<ManualClock>,
    pub org: Org,
    pub repo: Repo,
    pub public_repo: Repo,
    /// Whole-tree listings the security module asked its VCS port for.
    pub listings: Arc<AtomicUsize>,
}

/// The security module's VCS port, counting whole-tree listings.
struct CountingVcs {
    inner: HistorySecurityVcs,
    listings: Arc<AtomicUsize>,
}

#[async_trait]
impl SecurityVcs for CountingVcs {
    async fn resolve(&self, repo: &Repo, expr: &str) -> bee_forge::Result<ObjectId> {
        self.inner.resolve(repo, expr).await
    }
    async fn list_files(&self, repo: &Repo, at: &ObjectId) -> bee_forge::Result<Vec<TreeFile>> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        self.inner.list_files(repo, at).await
    }
    async fn read_file(
        &self,
        repo: &Repo,
        at: &ObjectId,
        path: &str,
    ) -> bee_forge::Result<Option<Vec<u8>>> {
        self.inner.read_file(repo, at, path).await
    }
    async fn added_blobs(&self, repo: &Repo, at: &ObjectId) -> bee_forge::Result<Vec<AddedBlob>> {
        self.inner.added_blobs(repo, at).await
    }
    async fn history(
        &self,
        repo: &Repo,
        tip: &ObjectId,
        limit: usize,
    ) -> bee_forge::Result<Vec<ObjectId>> {
        self.inner.history(repo, tip, limit).await
    }
    async fn changed_paths(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
    ) -> bee_forge::Result<Option<Vec<String>>> {
        self.inner.changed_paths(repo, from, to).await
    }
}

pub async fn principal(d: &MemoryDirectory, id: &str) -> Principal {
    d.get(id).await.unwrap()
}

pub async fn world() -> World {
    world_on(Arc::new(MemoryStore::new())).await
}

pub async fn world_on(store: Arc<dyn ForgeStore>) -> World {
    let clock = Arc::new(ManualClock::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    ));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(dyn_clock.clone()));
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
    let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta, dyn_clock.clone())),
        ..Hooks::default()
    };
    let (forge, _) = Forge::open(
        store,
        dyn_clock.clone(),
        ring.verifying_keys(),
        Arc::new(InMemoryRevocations::new()),
        hooks,
        crate::common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer.clone(), dyn_clock.clone()));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    changes.attach(&forge);
    let feed = Arc::new(MemoryFeed::new());
    let registry = Arc::new(MemoryRegistry::new());
    let verifier = Arc::new(TestVerifier::default());
    let sys = Cx::new("owner", PrincipalKind::Human);
    let name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
    let org = forge
        .orgs
        .create_org(&sys, &name, "", principal(&directory, "owner").await)
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    let mut repos = Vec::new();
    for (n, vis) in [("app", Visibility::Private), ("libpub", Visibility::Public)] {
        let repo = forge
            .orgs
            .create_repo(
                &sys,
                &org.id,
                NewRepo {
                    id: forge.db.ids().ulid().unwrap(),
                    name: n.into(),
                    visibility: vis,
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
        repos.push(repo);
    }
    let public_repo = repos.pop().unwrap();
    let repo = repos.pop().unwrap();
    for (who, role, org_role) in [
        ("maintainer", RepoRole::Maintain, Role::Member),
        ("writer", RepoRole::Write, Role::Member),
        ("reader", RepoRole::Read, Role::Member),
        ("ci", RepoRole::Write, Role::Member),
        ("agent", RepoRole::Write, Role::Member),
        ("reviewer", RepoRole::Write, Role::Member),
        ("bee-update-bot", RepoRole::Write, Role::Member),
    ] {
        let p = principal(&directory, who).await;
        forge
            .orgs
            .set_member(&sys, &org.id, p.clone(), org_role)
            .await
            .unwrap();
        for r in [&repo, &public_repo] {
            forge
                .orgs
                .add_collaborator(&sys, &r.id, p.clone(), role, "")
                .await
                .unwrap();
        }
    }
    forge
        .orgs
        .set_member(
            &sys,
            &org.id,
            principal(&directory, "secmgr").await,
            Role::SecurityManager,
        )
        .await
        .unwrap();
    let listings = Arc::new(AtomicUsize::new(0));
    let ports = Ports {
        vcs: Arc::new(CountingVcs {
            inner: HistorySecurityVcs::new(writer),
            listings: listings.clone(),
        }),
        feed: feed.clone(),
        verifier: verifier.clone(),
        registry: registry.clone(),
        forks: Some(Arc::new(TestForks {
            forge: forge.clone(),
            org_id: org.id.clone(),
        })),
        ..Ports::default()
    };
    let sec = Security::new(&forge, Some(changes.clone()), ports);
    sec.attach(&forge);
    let svc = SecurityServices::new(forge.clone(), sec.clone());
    let w = World {
        forge,
        changes,
        vcs,
        sec,
        svc,
        feed,
        registry,
        verifier,
        directory,
        ring,
        clock,
        org,
        repo,
        public_repo,
        listings,
    };
    // The root changesets: a README only; tests add what they need.
    for r in [&w.repo, &w.public_repo] {
        let root = w
            .commit_in(r, None, &[("README.md", Some("# app\n"))], "root", "owner")
            .await;
        w.vcs.set_ref(r, "main", &root).await.unwrap();
    }
    w
}

impl World {
    pub fn cx(&self, id: &str) -> Cx {
        let kind = PEOPLE
            .iter()
            .find(|(p, _)| *p == id)
            .map(|(_, k)| *k)
            .unwrap_or(PrincipalKind::Human);
        let mut cx = Cx::new(id, kind);
        cx.decision = "test".into();
        cx
    }

    pub fn token(&self, id: &str, scope: Scope) -> String {
        let principal = match id {
            "ci" => bee_auth::Principal::service(id, id),
            _ => bee_auth::Principal::human(id, id),
        };
        self.ring
            .mint(
                &MintRequest {
                    principal,
                    on_behalf_of: vec![],
                    scope,
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    pub fn full(&self, id: &str) -> String {
        self.token(id, Scope::new())
    }

    pub fn scoped(&self, id: &str, actions: &[Action]) -> String {
        self.token(id, Scope::new().actions(actions.iter().copied()))
    }

    pub fn rref(&self) -> Option<RepoRef> {
        Some(RepoRef {
            repo: Some(repo_ref::Repo::Id(self.repo.id.clone())),
        })
    }

    pub fn pub_ref(&self) -> Option<RepoRef> {
        Some(RepoRef {
            repo: Some(repo_ref::Repo::Id(self.public_repo.id.clone())),
        })
    }

    pub async fn commit_in(
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
                    author: who.to_string(),
                    creator: who.to_string(),
                    chain: vec![who.to_string()],
                    change_id: None,
                },
            )
            .await
            .unwrap()
    }

    pub async fn tip_of(&self, repo: &Repo) -> ObjectId {
        self.vcs
            .ref_tip(repo, "main")
            .await
            .unwrap()
            .unwrap()
            .target
            .unwrap()
    }

    pub async fn tip(&self) -> ObjectId {
        self.tip_of(&self.repo).await
    }

    /// Commits `edits` on top of main of `repo` and moves main (a landed change).
    pub async fn push_to(
        &self,
        repo: &Repo,
        edits: &[(&str, Option<&str>)],
        message: &str,
    ) -> ObjectId {
        let tip = self.tip_of(repo).await;
        let c = self
            .commit_in(repo, Some(&tip), edits, message, "owner")
            .await;
        self.vcs.set_ref(repo, "main", &c).await.unwrap();
        c
    }

    pub async fn push(&self, edits: &[(&str, Option<&str>)], message: &str) -> ObjectId {
        self.push_to(&self.repo, edits, message).await
    }

    pub async fn file(&self, at: &ObjectId, path: &str) -> Option<String> {
        self.vcs
            .read_file(&self.repo, at, path)
            .await
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap())
    }

    /// Plants the four manifests with their lockfiles on main.
    pub async fn plant_project(&self) -> ObjectId {
        self.push(
            &[
                ("package.json", Some(PACKAGE_JSON)),
                ("package-lock.json", Some(PACKAGE_LOCK)),
                ("Cargo.toml", Some(CARGO_TOML)),
                ("Cargo.lock", Some(CARGO_LOCK)),
                ("requirements.txt", Some(REQUIREMENTS)),
                ("go.mod", Some(GO_MOD)),
            ],
            "Add the project",
        )
        .await
    }

    /// Loads the four advisories into the feed and refreshes the index.
    pub async fn load_advisories(&self) {
        self.feed.set(advisories());
        self.sec
            .refresh_advisories(&self.cx("owner"))
            .await
            .unwrap();
    }

    /// Evaluates the default branch tip now.
    pub async fn evaluate(&self) -> bool {
        let tip = self.tip().await;
        self.sec
            .on_default_branch_moved(&Cx::system(), &self.repo, &tip)
            .await
            .unwrap()
    }

    pub async fn kinds(&self) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&self.cx("owner"), &self.repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    pub async fn audit_actions(&self) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (entries, _) = self
            .forge
            .audit
            .query(
                &self.cx("owner"),
                &bee_forge::audit::org_scope(&self.org.id),
                &Default::default(),
                false,
                "",
                500,
            )
            .await
            .unwrap();
        entries.into_iter().map(|e| e.action).collect()
    }

    pub async fn enable_security_updates(&self) {
        self.sec
            .update_config(
                &self.cx("owner"),
                &self.repo,
                &UpdateSecurityConfigurationRequest {
                    security_updates: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
}

/// A gzip of `bytes` (for SARIF uploads).
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}
