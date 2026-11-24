//! The world the briefs tests share (`briefs_brief`, `briefs_evidence`): a forge on the memory
//! or SQLite store, change requests over real history (`HistoryVcs` on in-memory VCS stores),
//! the briefs module with in-memory CI blobs, and tokens for every test principal.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::briefs::service::BriefServices;
use bee_forge::briefs::{ArtifactSource, BriefPorts, Briefs};
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, HistoryVcs, MemoryTeams, NewChange};
use bee_forge::ci::ports::{MemoryCiBlobs, UrlSigner};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PrincipalDirectory};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

use crate::common;

pub const T0: u64 = 1_800_000_000_000;
pub const BASE: &str = "https://hub.example";

pub const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("alice", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("ci", PrincipalKind::Service),
    ("agent", PrincipalKind::Agent),
];

/// Which store the world runs on.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    Memory,
    Sqlite,
}

/// Workflow artifacts a test hands out: `(artifact, bytes)` by id.
#[derive(Default)]
pub struct FakeArtifacts {
    pub items: Mutex<Vec<(WorkflowArtifact, Vec<u8>)>>,
}

#[async_trait]
impl ArtifactSource for FakeArtifacts {
    async fn artifact(
        &self,
        _cx: &Cx,
        _repo: &Repo,
        id: &str,
    ) -> bee_forge::Result<WorkflowArtifact> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|(a, _)| a.id == id)
            .map(|(a, _)| a.clone())
            .ok_or_else(|| bee_forge::ForgeError::not_found(format!("artifact {id}")))
    }

    async fn artifact_bytes(&self, _cx: &Cx, _repo: &Repo, id: &str) -> bee_forge::Result<Vec<u8>> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|(a, _)| a.id == id)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| bee_forge::ForgeError::not_found(format!("artifact {id}")))
    }
}

pub struct World {
    pub forge: Arc<Forge>,
    pub changes: Arc<Changes>,
    pub briefs: Arc<Briefs>,
    pub vcs: Arc<HistoryVcs>,
    pub clock: Arc<ManualClock>,
    pub ring: bee_auth::KeyRing,
    pub blobs: Arc<MemoryCiBlobs>,
    pub artifacts: Arc<FakeArtifacts>,
    pub org: Org,
    /// Private; writer, alice, agent and ci write, reader reads.
    pub app: Repo,
    /// Public; owner only.
    pub open: Repo,
    pub b: BriefServices,
    pub c: ChangeServices,
    _dir: Option<tempfile::TempDir>,
}

pub async fn world(backend: Backend) -> World {
    let clock = Arc::new(ManualClock::new(T0));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let (store, dir): (Arc<dyn ForgeStore>, Option<tempfile::TempDir>) = match backend {
        Backend::Memory => (Arc::new(MemoryStore::new()), None),
        Backend::Sqlite => {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = SqlStore::open(SqlConfig::sqlite(dir.path()), dyn_clock.clone())
                .await
                .expect("open sqlite");
            (Arc::new(store), Some(dir))
        }
    };
    let meta = Arc::new(MemoryMetaStore::new());
    let vblobs = Arc::new(MemoryBlobStore::new(dyn_clock.clone()));
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
    let (forge, _) = Forge::open(
        store,
        dyn_clock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Hooks {
            directory: directory.clone(),
            provisioner: Arc::new(MetaProvisioner::new(meta, dyn_clock.clone())),
            ..Hooks::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer, dyn_clock.clone()));
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
    let blobs = Arc::new(MemoryCiBlobs::new());
    let artifacts = Arc::new(FakeArtifacts::default());
    let briefs = Briefs::new(
        &forge,
        changes.clone(),
        BriefPorts {
            blobs: blobs.clone(),
            urls: UrlSigner::new(BASE, &[5u8; 32]),
            artifacts: Some(artifacts.clone()),
        },
    );
    let owner_cx = Cx::new("owner", PrincipalKind::Human);
    let owner = directory.get("owner").await.unwrap();
    let org = forge
        .orgs
        .create_org(&owner_cx, "acme", "", owner)
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    let mut repos = Vec::new();
    for (name, visibility) in [("app", Visibility::Private), ("open", Visibility::Public)] {
        let repo = forge
            .orgs
            .create_repo(
                &owner_cx,
                &org.id,
                NewRepo {
                    id: forge.db.ids().ulid().unwrap(),
                    name: name.into(),
                    visibility,
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
        let root = vcs
            .create_commit(
                &repo,
                NewCommit {
                    parent: None,
                    edits: vec![("README.md".into(), Some(b"# hello\n".to_vec()))],
                    message: "root".into(),
                    author: "owner".into(),
                    creator: "owner".into(),
                    chain: vec!["owner".into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        vcs.set_ref(&repo, "main", &root).await.unwrap();
        repos.push(repo);
    }
    let (app, open) = (repos[0].clone(), repos[1].clone());
    for who in ["reader", "writer", "alice", "agent", "ci"] {
        forge
            .orgs
            .set_member(
                &owner_cx,
                &org.id,
                directory.get(who).await.unwrap(),
                Role::Member,
            )
            .await
            .unwrap();
    }
    for who in ["writer", "alice", "agent", "ci"] {
        forge
            .orgs
            .add_collaborator(
                &owner_cx,
                &app.id,
                directory.get(who).await.unwrap(),
                RepoRole::Write,
                "",
            )
            .await
            .unwrap();
    }
    forge
        .orgs
        .add_collaborator(
            &owner_cx,
            &app.id,
            directory.get("reader").await.unwrap(),
            RepoRole::Read,
            "",
        )
        .await
        .unwrap();
    World {
        b: BriefServices::new(forge.clone(), briefs.clone()),
        c: ChangeServices::new(forge.clone(), changes.clone()),
        forge,
        changes,
        briefs,
        vcs,
        clock,
        ring,
        blobs,
        artifacts,
        org,
        app,
        open,
        _dir: dir,
    }
}

/// Runs `async fn $name(World)` on both stores as `$name::memory` and `$name::sqlite`.
#[macro_export]
macro_rules! briefs_on_both {
    ($name:ident) => {
        mod $name {
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn memory() {
                super::$name(
                    $crate::briefs_support::world($crate::briefs_support::Backend::Memory).await,
                )
                .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn sqlite() {
                super::$name(
                    $crate::briefs_support::world($crate::briefs_support::Backend::Sqlite).await,
                )
                .await
            }
        }
    };
}

pub fn rref(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(repo.full_name.clone())),
    })
}

/// `REASON/subcode` of a failed call.
pub fn fail<T: std::fmt::Debug>(
    r: std::result::Result<tonic::Response<T>, tonic::Status>,
) -> String {
    let st = r.expect_err("expected failure");
    bee_proto::errors::detail_from_status(&st)
        .map(|d| format!("{}/{}", d.reason, d.subcode))
        .unwrap_or_else(|| format!("no detail: {st:?}"))
}

/// A minimal PNG of `w` × `h` (signature and header: what sniffing and sizing read).
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

pub fn sha(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub fn kind_of(id: &str) -> PrincipalKind {
    PEOPLE
        .iter()
        .find(|(p, _)| *p == id)
        .map(|(_, k)| *k)
        .unwrap_or(PrincipalKind::Human)
}

impl World {
    pub fn cx(&self, id: &str) -> Cx {
        Cx::new(id, kind_of(id))
    }

    pub fn tok(&self, id: &str) -> String {
        let (principal, on_behalf_of) = match kind_of(id) {
            PrincipalKind::Service => (bee_auth::Principal::service(id, id), vec![]),
            PrincipalKind::Agent => (
                bee_auth::Principal::agent(
                    id,
                    id,
                    bee_auth::AgentInfo {
                        owner: bee_policy::PrincipalId::new("alice"),
                        runtime: "claude-code".into(),
                        purpose: "tests".into(),
                    },
                ),
                vec![bee_auth::PrincipalRef::new(
                    "alice",
                    bee_policy::PrincipalKind::Human,
                )],
            ),
            _ => (bee_auth::Principal::human(id, id), vec![]),
        };
        self.ring
            .mint(
                &bee_auth::MintRequest {
                    principal,
                    on_behalf_of,
                    scope: bee_auth::Scope::new(),
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    pub fn tick(&self) {
        self.clock.advance(1_000);
    }

    async fn commit_on(
        &self,
        repo: &Repo,
        parent: ObjectId,
        who: &str,
        edits: &[(&str, &str)],
    ) -> ObjectId {
        self.vcs
            .create_commit(
                repo,
                NewCommit {
                    parent: Some(parent),
                    edits: edits
                        .iter()
                        .map(|(p, c)| (p.to_string(), Some(c.as_bytes().to_vec())))
                        .collect(),
                    message: format!("edit {}", edits[0].0),
                    author: who.into(),
                    creator: who.into(),
                    chain: vec![who.into()],
                    change_id: None,
                },
            )
            .await
            .unwrap()
    }

    /// The head commit of a CR that edits `edits` on top of `main`.
    pub async fn head(&self, repo: &Repo, who: &str, edits: &[(&str, &str)]) -> ObjectId {
        let tip = self
            .vcs
            .ref_tip(repo, "main")
            .await
            .unwrap()
            .unwrap()
            .target
            .unwrap();
        self.commit_on(repo, tip, who, edits).await
    }

    /// Opens a CR on `repo` as `who`, editing `edits`.
    pub async fn open_cr(&self, who: &str, repo: &Repo, edits: &[(&str, &str)]) -> ChangeRequest {
        let head = self.head(repo, who, edits).await;
        self.tick();
        self.changes
            .open(
                &self.cx(who),
                repo,
                NewChange {
                    head: Some(head),
                    title: format!("Change {}", edits[0].0),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap()
    }

    /// Pushes a new revision editing `edits` on top of the CR's head.
    pub async fn revise(
        &self,
        who: &str,
        repo: &Repo,
        cr: &ChangeRequest,
        edits: &[(&str, &str)],
    ) -> ChangeRequest {
        let cur = self
            .changes
            .get(&self.cx("owner"), repo, &cr.id)
            .await
            .unwrap();
        let head = cur.revisions.last().unwrap().head.clone().unwrap();
        let next = self.commit_on(repo, head, who, edits).await;
        self.tick();
        self.changes
            .revise(&self.cx(who), repo, &cr.id, &next)
            .await
            .unwrap()
    }

    /// Event kinds of a repo, oldest first.
    pub async fn kinds(&self, repo: &Repo) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&self.cx("owner"), &repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    /// Audit actions of the org, oldest first.
    pub async fn audit(&self) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (entries, _) = self
            .forge
            .audit
            .query(
                &self.cx("owner"),
                &format!("org:{}", self.org.id),
                &AuditLogFilter::default(),
                true,
                "",
                1000,
            )
            .await
            .unwrap();
        entries.into_iter().map(|e| e.action).collect()
    }

    pub fn shot(&self, title: &str) -> EvidenceSpec {
        EvidenceSpec {
            kind: EvidenceKind::Screenshot as i32,
            title: title.into(),
            ..Default::default()
        }
    }

    /// Adds a screenshot inline as `who`.
    pub async fn add_shot(
        &self,
        who: &str,
        repo: &Repo,
        cr: &ChangeRequest,
        title: &str,
    ) -> std::result::Result<tonic::Response<Evidence>, tonic::Status> {
        use bee_proto::v1::brief_service_server::BriefService;
        self.b
            .add_evidence(common::req(
                &self.tok(who),
                AddEvidenceRequest {
                    repo: rref(repo),
                    change_id: cr.id.clone(),
                    spec: Some(self.shot(title)),
                    assets: vec![InlineEvidenceAsset {
                        role: EvidenceAssetRole::Main as i32,
                        content_type: "image/png".into(),
                        data: png(1440, 900),
                        alt: "The checkout page".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ))
            .await
    }

    /// The path and query of a signed URL.
    pub fn target(url: &str) -> String {
        url.strip_prefix(BASE).unwrap_or(url).to_string()
    }
}
