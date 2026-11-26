//! Crate-private stored records. Wire resources (`bee.v1.DependencyAlert`,
//! `SecretScanningAlert`, ...) are stored as they are inside these wrappers, which add what the
//! wire messages do not carry. Records inherit the additive-only rule: never renumber or reuse
//! a field.

use bee_proto::v1::{
    Advisory, CodeScanningAlert, CodeScanningAlertInstance, CodeScanningSarifUpload,
    DependencyAlert, DependencyManifest, DependencySubmittedManifest, DependencyUpdateRun,
    PushProtectionBypass, SecretFinding, SecretScanningAlert, SecretScanningLocation,
    SecurityCodeQualityFinding, SecurityRepoAdvisory,
};
use prost::Message;

/// One package of a stored dependency graph.
#[derive(Clone, PartialEq, Message)]
pub struct PkgRec {
    /// Ecosystem proto name (`npm`, `cargo`, `pip`, ...).
    #[prost(string, tag = "1")]
    pub ecosystem: String,
    /// Package name.
    #[prost(string, tag = "2")]
    pub name: String,
    /// Version.
    #[prost(string, tag = "3")]
    pub version: String,
    /// Package URL.
    #[prost(string, tag = "4")]
    pub purl: String,
    /// Declaring file.
    #[prost(string, tag = "5")]
    pub source_path: String,
    /// `bee.v1.DependencyScope`.
    #[prost(int32, tag = "6")]
    pub scope: i32,
    /// `bee.v1.DependencyRelationship`.
    #[prost(int32, tag = "7")]
    pub relationship: i32,
    /// Version came from a lock or exact pin.
    #[prost(bool, tag = "8")]
    pub resolved: bool,
    /// Purls this package depends on.
    #[prost(string, repeated, tag = "9")]
    pub dependencies: Vec<String>,
    /// SPDX licenses.
    #[prost(string, repeated, tag = "10")]
    pub licenses: Vec<String>,
    /// Declared requirement.
    #[prost(string, tag = "11")]
    pub requirement: String,
    /// Integrity string.
    #[prost(string, tag = "12")]
    pub integrity: String,
    /// Came from the submission API.
    #[prost(bool, tag = "13")]
    pub submitted: bool,
}

/// The last evaluated dependency graph of a repo's default branch.
#[derive(Clone, PartialEq, Message)]
pub struct GraphRec {
    /// Changeset the graph was built at (raw id).
    #[prost(bytes = "vec", tag = "1")]
    pub changeset: Vec<u8>,
    /// Ref.
    #[prost(string, tag = "2")]
    pub ref_name: String,
    /// Digest of the dependency files: an unchanged digest skips re-parsing.
    #[prost(string, tag = "3")]
    pub digest: String,
    /// Packages.
    #[prost(message, repeated, tag = "4")]
    pub packages: Vec<PkgRec>,
    /// Detected manifests.
    #[prost(message, repeated, tag = "5")]
    pub manifests: Vec<DependencyManifest>,
    /// Package names (`<ecosystem>:<name>`) the repo itself publishes.
    #[prost(string, repeated, tag = "6")]
    pub published: Vec<String>,
    /// Update time.
    #[prost(uint64, tag = "7")]
    pub updated_ms: u64,
    /// Advisory-index generation the findings were computed against.
    #[prost(uint64, tag = "8")]
    pub advisory_generation: u64,
    /// Paths of the dependency files at `changeset`, sorted (valid when `paths_recorded`).
    #[prost(string, repeated, tag = "9")]
    pub dependency_paths: Vec<String>,
    /// Digest of the dependency files alone (without submitted snapshots).
    #[prost(string, tag = "10")]
    pub files_digest: String,
    /// Whether `dependency_paths` and `files_digest` were recorded (graphs stored before they
    /// existed lack them and are re-listed once).
    #[prost(bool, tag = "11")]
    pub paths_recorded: bool,
}

/// A vulnerability alert with what the wire message omits.
#[derive(Clone, PartialEq, Message)]
pub struct DepAlertRec {
    /// The alert.
    #[prost(message, optional, tag = "1")]
    pub alert: Option<DependencyAlert>,
    /// Versions that fix the advisory, newer than the installed one.
    #[prost(string, repeated, tag = "2")]
    pub fixed_versions: Vec<String>,
    /// Shortest dependency path from a direct dependency (purls).
    #[prost(string, repeated, tag = "3")]
    pub dependency_path: Vec<String>,
    /// Dedupe key `<purl>|<manifest>|<advisory id>`.
    #[prost(string, tag = "4")]
    pub key: String,
    /// Advisory id (also in the alert's advisory).
    #[prost(string, tag = "5")]
    pub advisory_id: String,
    /// Other ids of the advisory (CVE, GHSA, vendor).
    #[prost(string, repeated, tag = "6")]
    pub aliases: Vec<String>,
    /// The alert's change request id (for state checks).
    #[prost(string, tag = "7")]
    pub update_cr_id: String,
}

/// A secret alert with its locations. The secret is never stored, only a keyed hash.
#[derive(Clone, PartialEq, Message)]
pub struct SecretAlertRec {
    /// The alert.
    #[prost(message, optional, tag = "1")]
    pub alert: Option<SecretScanningAlert>,
    /// Where it appears.
    #[prost(message, repeated, tag = "2")]
    pub locations: Vec<SecretScanningLocation>,
    /// Keyed hash of the secret (hex).
    #[prost(string, tag = "3")]
    pub secret_hash: String,
    /// Dedupe key `<secret_type>:<hash>`.
    #[prost(string, tag = "4")]
    pub key: String,
}

/// A push-protection block: what the server refused and who was refused.
#[derive(Clone, PartialEq, Message)]
pub struct BlockRec {
    /// Block id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Blocking findings (no secret text).
    #[prost(message, repeated, tag = "2")]
    pub findings: Vec<SecretFinding>,
    /// Refused principal.
    #[prost(string, tag = "3")]
    pub requester: String,
    /// Creation time.
    #[prost(uint64, tag = "4")]
    pub created_ms: u64,
    /// Expiry.
    #[prost(uint64, tag = "5")]
    pub expires_ms: u64,
    /// Redacted previews, parallel to `findings`.
    #[prost(string, repeated, tag = "6")]
    pub previews: Vec<String>,
    /// Keyed secret hashes, parallel to `findings`.
    #[prost(string, repeated, tag = "7")]
    pub hashes: Vec<String>,
}

/// A bypass request with its consumption state.
#[derive(Clone, PartialEq, Message)]
pub struct BypassRec {
    /// The request.
    #[prost(message, optional, tag = "1")]
    pub bypass: Option<PushProtectionBypass>,
    /// The bypass was used by a write.
    #[prost(bool, tag = "2")]
    pub consumed: bool,
}

/// A code scanning alert with its dedupe key.
#[derive(Clone, PartialEq, Message)]
pub struct CodeAlertRec {
    /// The alert.
    #[prost(message, optional, tag = "1")]
    pub alert: Option<CodeScanningAlert>,
    /// `<tool>|<rule>|<fingerprint>`.
    #[prost(string, tag = "2")]
    pub key: String,
    /// The revision of the first observation, retained across rescans and reopening.
    #[prost(message, optional, tag = "3")]
    pub first_changeset: Option<bee_proto::v1::ObjectId>,
}

/// An instance of a code scanning alert on one ref and category.
#[derive(Clone, PartialEq, Message)]
pub struct InstanceRec {
    /// The instance.
    #[prost(message, optional, tag = "1")]
    pub instance: Option<CodeScanningAlertInstance>,
}

/// A code quality finding with its dedupe key.
#[derive(Clone, PartialEq, Message)]
pub struct QualityRec {
    /// The finding.
    #[prost(message, optional, tag = "1")]
    pub finding: Option<SecurityCodeQualityFinding>,
    /// Dedupe key.
    #[prost(string, tag = "2")]
    pub key: String,
}

/// A SARIF upload; the document is kept until processed.
#[derive(Clone, PartialEq, Message)]
pub struct SarifRec {
    /// The upload.
    #[prost(message, optional, tag = "1")]
    pub upload: Option<CodeScanningSarifUpload>,
    /// Gzip document, cleared once processed.
    #[prost(bytes = "vec", tag = "2")]
    pub sarif_gzip: Vec<u8>,
    /// Analysis start (ms), 0 = unknown.
    #[prost(uint64, tag = "3")]
    pub started_ms: u64,
    /// Checkout URI prefix to strip from locations.
    #[prost(string, tag = "4")]
    pub checkout_uri: String,
    /// Principal who uploaded.
    #[prost(string, tag = "5")]
    pub uploader: String,
    /// Tool name the request set (empty = from the SARIF).
    #[prost(string, tag = "6")]
    pub tool_override: String,
    /// Category the request set (empty = from the SARIF).
    #[prost(string, tag = "7")]
    pub category_override: String,
}

/// A stored dependency snapshot from the submission API.
#[derive(Clone, PartialEq, Message)]
pub struct SnapshotRec {
    /// Snapshot id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Detector name.
    #[prost(string, tag = "2")]
    pub detector: String,
    /// Detector version.
    #[prost(string, tag = "3")]
    pub detector_version: String,
    /// Job correlator.
    #[prost(string, tag = "4")]
    pub correlator: String,
    /// Job id.
    #[prost(string, tag = "5")]
    pub job_id: String,
    /// Changeset (raw).
    #[prost(bytes = "vec", tag = "6")]
    pub changeset: Vec<u8>,
    /// Ref.
    #[prost(string, tag = "7")]
    pub ref_name: String,
    /// Manifests.
    #[prost(message, repeated, tag = "8")]
    pub manifests: Vec<DependencySubmittedManifest>,
    /// Scan time (ms).
    #[prost(uint64, tag = "9")]
    pub scanned_ms: u64,
    /// Total packages.
    #[prost(uint32, tag = "10")]
    pub dependency_count: u32,
}

/// A repo advisory.
#[derive(Clone, PartialEq, Message)]
pub struct AdvisoryRec {
    /// The advisory.
    #[prost(message, optional, tag = "1")]
    pub advisory: Option<SecurityRepoAdvisory>,
}

/// An advisory of the global database that did not come from the feed (published from a repo).
#[derive(Clone, PartialEq, Message)]
pub struct GlobalAdvRec {
    /// The wire form.
    #[prost(message, optional, tag = "1")]
    pub advisory: Option<Advisory>,
    /// The OSV record matching uses.
    #[prost(string, tag = "2")]
    pub osv_json: String,
}

/// A CVE request record.
#[derive(Clone, PartialEq, Message)]
pub struct CveRequestRec {
    /// Request id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// The advisory.
    #[prost(string, tag = "2")]
    pub ghsa_id: String,
    /// Repo.
    #[prost(string, tag = "3")]
    pub repo_id: String,
    /// Requesting principal.
    #[prost(string, tag = "4")]
    pub requested_by: String,
    /// Time (ms).
    #[prost(uint64, tag = "5")]
    pub requested_ms: u64,
    /// `pending`, `assigned` or `rejected`.
    #[prost(string, tag = "6")]
    pub status: String,
    /// The assigned CVE id.
    #[prost(string, tag = "7")]
    pub cve_id: String,
}

/// An update run.
#[derive(Clone, PartialEq, Message)]
pub struct UpdateRunRec {
    /// The run.
    #[prost(message, optional, tag = "1")]
    pub run: Option<DependencyUpdateRun>,
    /// Principal that started it.
    #[prost(string, tag = "2")]
    pub starter: String,
    /// Entry key `<ecosystem>|<directory>`.
    #[prost(string, tag = "3")]
    pub entry: String,
}

/// A change request opened by the update bot, remembered so one fix opens one CR.
#[derive(Clone, PartialEq, Message)]
pub struct UpdatePrRec {
    /// CR number.
    #[prost(uint64, tag = "1")]
    pub number: u64,
    /// CR id.
    #[prost(string, tag = "2")]
    pub cr_id: String,
    /// Creation time (ms).
    #[prost(uint64, tag = "3")]
    pub created_ms: u64,
    /// Whether it was a security update.
    #[prost(bool, tag = "4")]
    pub security: bool,
}

/// Per-repo update bookkeeping: when each entry last ran on schedule.
#[derive(Clone, PartialEq, Message)]
pub struct UpdateStateRec {
    /// Entry keys.
    #[prost(string, repeated, tag = "1")]
    pub entries: Vec<String>,
    /// Last scheduled run per entry (ms), parallel to `entries`.
    #[prost(uint64, repeated, tag = "2")]
    pub last_ms: Vec<u64>,
}

/// How bypass requests are decided in a repo.
#[derive(Clone, PartialEq, Message)]
pub struct BypassPolicyRec {
    /// `review` (default) or `allow`.
    #[prost(string, tag = "1")]
    pub mode: String,
    /// Principals who may review bypass requests besides repo admins and security managers.
    #[prost(string, repeated, tag = "2")]
    pub reviewer_ids: Vec<String>,
    /// Hours an approved bypass stays usable (0 = default).
    #[prost(uint32, tag = "3")]
    pub ttl_hours: u32,
}

/// A boolean setting row (validity checks).
#[derive(Clone, PartialEq, Message)]
pub struct FlagRec {
    /// The value.
    #[prost(bool, tag = "1")]
    pub enabled: bool,
}
