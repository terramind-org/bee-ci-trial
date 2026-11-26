//! What Pages needs from the rest of the system. Detached defaults fail with `UNAVAILABLE`
//! (or allow, for the deploy gate); in-memory implementations serve tests and `--dev`.
//!
//! | port | backed by |
//! |---|---|
//! | [`crate::community::ports::ContentVcs`] | the VCS: ref resolution, trees, file reads |
//! | [`crate::packages::BlobPort`] | the blob store, under a `pages/` key prefix |
//! | [`DnsResolver`] | a DNS resolver (TXT, CNAME, A, AAAA, CAA lookups) |
//! | [`AcmeIssuer`] | an ACME client (HTTP-01 or DNS-01) plus the certificate store |
//! | [`ArtifactSource`] | workflow artifacts |
//! | [`DeployGate`] | environment protection rules of the `github-pages` environment |

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_proto::v1::{ObjectId, Repo};
use bytes::Bytes;

use crate::community::ports::ContentVcs;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::Clock;
use crate::packages::BlobPort;

fn detached(what: &str) -> ForgeError {
    ForgeError::unavailable(format!("no {what} is configured on this forge"))
        .with_subcode("port_unavailable")
}

/// What DNS says about one host name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DnsAnswer {
    /// CNAME target, without a trailing dot.
    pub cname: Option<String>,
    /// A records.
    pub a: Vec<String>,
    /// AAAA records.
    pub aaaa: Vec<String>,
    /// CAA records as `<tag> <value>` (for example `issue letsencrypt.org`).
    pub caa: Vec<String>,
}

/// A DNS resolver.
#[async_trait]
pub trait DnsResolver: Send + Sync {
    /// Address, alias and CAA records of `host`.
    async fn lookup(&self, host: &str) -> Result<DnsAnswer>;
    /// TXT records of `name`; empty when there are none.
    async fn txt(&self, name: &str) -> Result<Vec<String>>;
}

/// No resolver configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedDns;

#[async_trait]
impl DnsResolver for DetachedDns {
    async fn lookup(&self, _: &str) -> Result<DnsAnswer> {
        Err(detached("DNS resolver"))
    }
    async fn txt(&self, _: &str) -> Result<Vec<String>> {
        Err(detached("DNS resolver"))
    }
}

/// An in-memory zone.
#[derive(Default)]
pub struct MemoryDns {
    zone: Mutex<BTreeMap<String, DnsAnswer>>,
    txt: Mutex<BTreeMap<String, Vec<String>>>,
}

impl MemoryDns {
    /// An empty zone.
    pub fn new() -> MemoryDns {
        MemoryDns::default()
    }

    fn answer(&self, host: &str, f: impl FnOnce(&mut DnsAnswer)) {
        if let Ok(mut z) = self.zone.lock() {
            f(z.entry(host.to_lowercase()).or_default());
        }
    }

    /// Sets the CNAME of `host`.
    pub fn set_cname(&self, host: &str, target: &str) {
        self.answer(host, |a| a.cname = Some(target.to_lowercase()));
    }

    /// Adds an A record.
    pub fn add_a(&self, host: &str, ip: &str) {
        self.answer(host, |a| a.a.push(ip.to_string()));
    }

    /// Adds a CAA record (`issue letsencrypt.org`).
    pub fn add_caa(&self, host: &str, record: &str) {
        self.answer(host, |a| a.caa.push(record.to_string()));
    }

    /// Publishes a TXT record.
    pub fn add_txt(&self, name: &str, value: &str) {
        if let Ok(mut t) = self.txt.lock() {
            t.entry(name.to_lowercase())
                .or_default()
                .push(value.to_string());
        }
    }

    /// Removes every record of `host` and its TXT records.
    pub fn clear(&self, host: &str) {
        if let Ok(mut z) = self.zone.lock() {
            z.remove(&host.to_lowercase());
        }
        if let Ok(mut t) = self.txt.lock() {
            t.remove(&host.to_lowercase());
        }
    }
}

#[async_trait]
impl DnsResolver for MemoryDns {
    async fn lookup(&self, host: &str) -> Result<DnsAnswer> {
        Ok(self
            .zone
            .lock()
            .map_err(|_| ForgeError::internal("memory dns poisoned"))?
            .get(&host.to_lowercase())
            .cloned()
            .unwrap_or_default())
    }
    async fn txt(&self, name: &str) -> Result<Vec<String>> {
        Ok(self
            .txt
            .lock()
            .map_err(|_| ForgeError::internal("memory dns poisoned"))?
            .get(&name.to_lowercase())
            .cloned()
            .unwrap_or_default())
    }
}

/// A certificate the ACME port issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedCert {
    /// Id at the certificate store.
    pub id: String,
    /// Domains on the certificate.
    pub domains: Vec<String>,
    /// Not-before, milliseconds since the epoch.
    pub issued_ms: u64,
    /// Not-after.
    pub expires_ms: u64,
}

/// An ACME client with its certificate store. The key material never reaches the forge: the
/// TLS terminator reads it from the same store.
#[async_trait]
pub trait AcmeIssuer: Send + Sync {
    /// Issues (or renews) a certificate for `domains`.
    async fn issue(&self, domains: &[String]) -> Result<IssuedCert>;
}

/// No ACME client configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedAcme;

#[async_trait]
impl AcmeIssuer for DetachedAcme {
    async fn issue(&self, _: &[String]) -> Result<IssuedCert> {
        Err(detached("ACME client"))
    }
}

/// An in-memory issuer: certificates last `lifetime_days`; [`MemoryAcme::fail_next`] makes the
/// next request fail like a rejected challenge.
pub struct MemoryAcme {
    clock: Arc<dyn Clock>,
    lifetime_days: u64,
    fail: Mutex<Option<String>>,
    issued: Mutex<Vec<IssuedCert>>,
}

impl MemoryAcme {
    /// An issuer reading `clock`, issuing 90-day certificates.
    pub fn new(clock: Arc<dyn Clock>) -> MemoryAcme {
        MemoryAcme {
            clock,
            lifetime_days: 90,
            fail: Mutex::new(None),
            issued: Mutex::new(Vec::new()),
        }
    }

    /// Fails the next issuance with `message`.
    pub fn fail_next(&self, message: &str) {
        if let Ok(mut f) = self.fail.lock() {
            *f = Some(message.to_string());
        }
    }

    /// Every certificate issued so far.
    pub fn issued(&self) -> Vec<IssuedCert> {
        self.issued.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl AcmeIssuer for MemoryAcme {
    async fn issue(&self, domains: &[String]) -> Result<IssuedCert> {
        if let Some(m) = self.fail.lock().ok().and_then(|mut f| f.take()) {
            return Err(ForgeError::unavailable(m).with_subcode("acme_failed"));
        }
        let now = self.clock.now_ms();
        let mut issued = self
            .issued
            .lock()
            .map_err(|_| ForgeError::internal("memory acme poisoned"))?;
        let c = IssuedCert {
            id: format!("cert-{}", issued.len() + 1),
            domains: domains.to_vec(),
            issued_ms: now,
            expires_ms: now + self.lifetime_days * 86_400_000,
        };
        issued.push(c.clone());
        Ok(c)
    }
}

/// A workflow artifact.
#[derive(Clone, Debug)]
pub struct Artifact {
    /// The archive: zip, tar or tar.gz.
    pub data: Bytes,
    /// The workflow run that produced it.
    pub run_id: String,
    /// The changeset it was built from, when known.
    pub changeset: Option<ObjectId>,
}

/// Reads workflow artifacts.
#[async_trait]
pub trait ArtifactSource: Send + Sync {
    /// The artifact `artifact_id` of `repo`; `NOT_FOUND` when absent or expired.
    async fn fetch(&self, repo: &Repo, artifact_id: &str) -> Result<Artifact>;
}

/// No artifact store configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedArtifacts;

#[async_trait]
impl ArtifactSource for DetachedArtifacts {
    async fn fetch(&self, _: &Repo, _: &str) -> Result<Artifact> {
        Err(detached("artifact store"))
    }
}

/// In-memory artifacts keyed by repo id and artifact id.
#[derive(Default)]
pub struct MemoryArtifacts {
    map: Mutex<BTreeMap<(String, String), Artifact>>,
}

impl MemoryArtifacts {
    /// An empty store.
    pub fn new() -> MemoryArtifacts {
        MemoryArtifacts::default()
    }

    /// Stores an artifact.
    pub fn put(&self, repo_id: &str, artifact_id: &str, a: Artifact) {
        if let Ok(mut m) = self.map.lock() {
            m.insert((repo_id.to_string(), artifact_id.to_string()), a);
        }
    }
}

#[async_trait]
impl ArtifactSource for MemoryArtifacts {
    async fn fetch(&self, repo: &Repo, artifact_id: &str) -> Result<Artifact> {
        self.map
            .lock()
            .map_err(|_| ForgeError::internal("memory artifacts poisoned"))?
            .get(&(repo.id.clone(), artifact_id.to_string()))
            .cloned()
            .ok_or_else(|| ForgeError::not_found(format!("artifact {artifact_id}")))
    }
}

/// Environment protection: may a deployment to `environment` proceed?
#[async_trait]
pub trait DeployGate: Send + Sync {
    /// `APPROVAL_REQUIRED` when the environment is protected and not yet approved.
    async fn check(&self, cx: &Cx, repo: &Repo, environment: &str) -> Result<()>;
}

/// No protection rules: every deployment proceeds.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenGate;

#[async_trait]
impl DeployGate for OpenGate {
    async fn check(&self, _: &Cx, _: &Repo, _: &str) -> Result<()> {
        Ok(())
    }
}

/// The ports of Pages. `vcs` and `blobs` default to the community VCS and the package blob
/// store when `None`.
#[derive(Clone)]
pub struct PagesPorts {
    /// File access to repos; `None` = the community content VCS.
    pub vcs: Option<Arc<dyn ContentVcs>>,
    /// Site content; `None` = the package blob store.
    pub blobs: Option<Arc<dyn BlobPort>>,
    /// DNS.
    pub dns: Arc<dyn DnsResolver>,
    /// Certificates.
    pub acme: Arc<dyn AcmeIssuer>,
    /// Workflow artifacts.
    pub artifacts: Arc<dyn ArtifactSource>,
    /// Environment protection.
    pub gate: Arc<dyn DeployGate>,
}

impl Default for PagesPorts {
    fn default() -> PagesPorts {
        PagesPorts {
            vcs: None,
            blobs: None,
            dns: Arc::new(DetachedDns),
            acme: Arc::new(DetachedAcme),
            artifacts: Arc::new(DetachedArtifacts),
            gate: Arc::new(OpenGate),
        }
    }
}
