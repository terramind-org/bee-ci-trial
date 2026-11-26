//! Shared setup for the packages and Pages tests: a forge with in-memory ports, an org `acme`
//! with a private repo `app` and a public repo `open`, and HTTP and archive helpers.
#![allow(dead_code, unused_imports, missing_docs)]

pub(crate) use crate::common;

use std::sync::Arc;

use bee_auth::{InMemoryRevocations, KeyRing, SigningKey};
use bee_forge::community::Ports;
use bee_forge::community::ports::{MemoryVcs, VcsRepo};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, RevisionResolver};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::packages::{MemoryBlobs, PackagePorts, PackageRegistry};
use bee_forge::pages::{MemoryAcme, MemoryArtifacts, MemoryDns, PagesPorts};
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::{Cx, Forge};
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::repo_service_server::RepoService;
pub use bee_proto::v1::*;
use bytes::Bytes;

pub use common::{actions, reason, req};

/// Start of the manual clock: 2026-10-18 12:00 UTC.
pub const T0: u64 = 1_792_324_800_000;

pub struct World {
    pub env: common::Env,
    pub s: ForgeServices,
    pub reg: PackageRegistry,
    pub vcs: Arc<MemoryVcs>,
    pub blobs: Arc<MemoryBlobs>,
    pub dns: Arc<MemoryDns>,
    pub acme: Arc<MemoryAcme>,
    pub artifacts: Arc<MemoryArtifacts>,
    pub gate: Arc<ToggleGate>,
    pub clock: Arc<ManualClock>,
    pub app: Repo,
    pub open: Repo,
}

pub fn rref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.into())),
    })
}

pub async fn world() -> World {
    world_on(Arc::new(MemoryStore::new())).await
}

pub async fn world_on(store: Arc<dyn ForgeStore>) -> World {
    let clock = Arc::new(ManualClock::new(T0));
    let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let revocations = Arc::new(InMemoryRevocations::new());
    let directory = Arc::new(MemoryDirectory::new());
    let vcs = Arc::new(MemoryVcs::new());
    let (pports, blobs) = PackagePorts::in_memory();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let dns = Arc::new(MemoryDns::new());
    let acme = Arc::new(MemoryAcme::new(clock_dyn.clone()));
    let artifacts = Arc::new(MemoryArtifacts::new());
    let gate = Arc::new(ToggleGate::default());
    let hooks = Hooks {
        directory: directory.clone(),
        community: Ports {
            vcs: vcs.clone(),
            tags: vcs.clone(),
            ..Ports::default()
        },
        packages: pports,
        pages: PagesPorts {
            dns: dns.clone(),
            acme: acme.clone(),
            gate: gate.clone(),
            artifacts: artifacts.clone(),
            ..PagesPorts::default()
        },
        ..Hooks::default()
    };
    let mut config = common::test_config();
    config.packages.base_url = "https://beehive.test".into();
    config.pages.pages_domain = "pages.test".into();
    config.packages.public_delete_download_limit = 3;
    config.pages.max_files = 8;
    let (forge, _) = Forge::open(
        store,
        clock_dyn.clone(),
        ring.verifying_keys(),
        revocations.clone(),
        hooks,
        config,
    )
    .await
    .expect("open forge");
    for (id, kind) in [
        ("owner", PrincipalKind::Human),
        ("admin", PrincipalKind::Human),
        ("writer", PrincipalKind::Human),
        ("reader", PrincipalKind::Human),
        ("outsider", PrincipalKind::Human),
        ("ci", PrincipalKind::Service),
        ("agent", PrincipalKind::Agent),
    ] {
        directory.register(
            Principal {
                id: id.into(),
                kind: kind as i32,
                display: id.into(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    let env = common::Env {
        forge: forge.clone(),
        ring,
        revocations,
        directory,
        clock: clock_dyn,
    };
    let s = ForgeServices::new(forge.clone());
    let owner = env.full("owner");
    s.create_org(req(
        &owner,
        CreateOrgRequest {
            name: "acme".into(),
            display: "Acme".into(),
        },
    ))
    .await
    .unwrap();
    let mk = |name: &str, vis: Visibility| {
        s.create_repo(req(
            &owner,
            CreateRepoRequest {
                org: "acme".into(),
                name: name.into(),
                visibility: vis as i32,
                ..Default::default()
            },
        ))
    };
    let app = mk("app", Visibility::Private).await.unwrap().into_inner();
    let open = mk("open", Visibility::Public).await.unwrap().into_inner();
    for (who, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("reader", Role::Member),
        ("ci", Role::Member),
        ("agent", Role::Member),
    ] {
        s.set_member(req(
            &owner,
            SetMemberRequest {
                org: "acme".into(),
                principal_id: who.into(),
                role: role as i32,
            },
        ))
        .await
        .unwrap();
    }
    for repo in ["acme/app", "acme/open"] {
        for (who, role) in [
            ("writer", RepoRole::Write),
            ("ci", RepoRole::Write),
            ("agent", RepoRole::Write),
        ] {
            s.add_collaborator(req(
                &owner,
                AddCollaboratorRequest {
                    repo: rref(repo),
                    principal_id: who.into(),
                    role: role as i32,
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
        }
    }
    World {
        reg: PackageRegistry::new(forge),
        env,
        s,
        vcs,
        blobs,
        dns,
        acme,
        artifacts,
        gate,
        clock,
        app,
        open,
    }
}

impl World {
    pub fn tok(&self, who: &str) -> String {
        self.env.full(who)
    }

    /// A token scoped to one repo with read and commit, as a workflow job gets.
    pub fn job_token(&self, repo: &Repo) -> String {
        self.env.token(
            "ci",
            bee_auth::Scope::new()
                .repos([repo.id.clone()])
                .unwrap()
                .actions([bee_policy::Action::Read, bee_policy::Action::Commit]),
        )
    }

    pub async fn http(&self, r: http::Request<Bytes>) -> http::Response<Bytes> {
        self.reg.handle(r).await
    }

    pub async fn flush(&self) {
        self.env.forge.flush().await.unwrap();
    }

    pub async fn event_kinds(&self) -> Vec<String> {
        let org = self
            .env
            .forge
            .orgs
            .get_org(&Cx::system(), "acme")
            .await
            .unwrap();
        let (events, _) = self
            .env
            .forge
            .events
            .read(
                &Cx::system(),
                &bee_forge::events::org_log(&org.id),
                0,
                &[],
                2000,
            )
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    pub fn vr(&self, repo: &Repo) -> VcsRepo {
        VcsRepo::of_repo(repo)
    }
}

/// An HTTP request.
pub fn rq(
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: impl Into<Bytes>,
) -> http::Request<Bytes> {
    let mut b = http::Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(body.into()).unwrap()
}

pub fn body_str(r: &http::Response<Bytes>) -> String {
    String::from_utf8_lossy(r.body()).into_owned()
}

pub fn body_json(r: &http::Response<Bytes>) -> serde_json::Value {
    serde_json::from_slice(r.body()).unwrap_or_else(|e| panic!("not JSON ({e}): {}", body_str(r)))
}

pub fn hdr<'a>(r: &'a http::Response<Bytes>, k: &str) -> &'a str {
    r.headers()
        .get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

// ---- archive builders ----

pub fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, body) in files {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        let size = format!("{:011o}", body.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[156] = b'0';
        h[257..262].copy_from_slice(b"ustar");
        out.extend_from_slice(&h);
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0u8; 1024]);
    out
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    bee_forge::packages::archive::gzip(data)
}

/// A zip with stored entries and, when `deflate` names an entry, that entry deflated.
pub fn zip(files: &[(&str, &[u8])], deflate: &str) -> Vec<u8> {
    use std::io::Write;
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for (name, body) in files {
        let mut crc = flate2::Crc::new();
        crc.update(body);
        let (method, data) = if *name == deflate {
            let mut e =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(body).unwrap();
            (8u16, e.finish().unwrap())
        } else {
            (0u16, body.to_vec())
        };
        let off = out.len() as u32;
        out.extend_from_slice(&[0x50, 0x4b, 3, 4, 20, 0, 0, 0]);
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);
        cd.extend_from_slice(&[0x50, 0x4b, 1, 2, 20, 0, 20, 0, 0, 0]);
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&[0, 0, 0, 0]);
        cd.extend_from_slice(&crc.sum().to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0u8; 12]);
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(&[0x50, 0x4b, 5, 6, 0, 0, 0, 0]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

pub fn sha256_digest(b: &[u8]) -> String {
    bee_forge::packages::hash::sha256_digest(b)
}

/// An environment gate that refuses while `protected` is set.
#[derive(Default)]
pub struct ToggleGate {
    pub protected: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl bee_forge::pages::DeployGate for ToggleGate {
    async fn check(&self, _: &Cx, _: &Repo, env: &str) -> bee_forge::error::Result<()> {
        if self.protected.load(std::sync::atomic::Ordering::SeqCst) {
            Err(bee_forge::ForgeError::new(
                ErrorReason::ApprovalRequired,
                format!("environment {env} needs approval"),
            ))
        } else {
            Ok(())
        }
    }
}

impl World {
    /// The verified caller of `who`'s token, as the server's session layer would build it.
    pub fn caller(&self, who: &str) -> bee_forge::auth::Caller {
        let mut r = tonic::Request::new(());
        r.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.tok(who)).parse().unwrap(),
        );
        self.env.forge.authn.authenticate(&r).unwrap()
    }
}
