//! Stored records of the pages surface (crate-private protobuf messages).

/// A repo's site.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SiteRec {
    /// Repo id.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// Org id.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// [`bee_proto::v1::PagesBuildType`].
    #[prost(int32, tag = "3")]
    pub build_type: i32,
    /// Source ref of a REF site.
    #[prost(string, tag = "4")]
    pub source_ref: String,
    /// Source folder of a REF site (`/` or `/docs`).
    #[prost(string, tag = "5")]
    pub source_path: String,
    /// [`bee_proto::v1::PagesAccess`].
    #[prost(int32, tag = "6")]
    pub access: i32,
    /// Custom domain, lower case, empty for none.
    #[prost(string, tag = "7")]
    pub custom_domain: String,
    /// Redirect plain HTTP to HTTPS.
    #[prost(bool, tag = "8")]
    pub enforce_https: bool,
    /// [`bee_proto::v1::PagesCertificateState`].
    #[prost(int32, tag = "9")]
    pub cert_state: i32,
    /// Domains on the certificate.
    #[prost(string, repeated, tag = "10")]
    pub cert_domains: Vec<String>,
    /// Certificate error.
    #[prost(string, tag = "11")]
    pub cert_error: String,
    /// Certificate issue time.
    #[prost(uint64, tag = "12")]
    pub cert_issued_ms: u64,
    /// Certificate expiry.
    #[prost(uint64, tag = "13")]
    pub cert_expires_ms: u64,
    /// Certificate id at the ACME port.
    #[prost(string, tag = "14")]
    pub cert_ref: String,
    /// The deployment being served.
    #[prost(string, tag = "15")]
    pub current_deployment_id: String,
    /// Tree id served by a REF site.
    #[prost(bytes = "vec", tag = "16")]
    pub served_tree: Vec<u8>,
    /// [`bee_proto::v1::PagesSiteStatus`].
    #[prost(int32, tag = "17")]
    pub status: i32,
    /// Creation time.
    #[prost(uint64, tag = "18")]
    pub created_ms: u64,
    /// Last change.
    #[prost(uint64, tag = "19")]
    pub updated_ms: u64,
    /// Preview deployments per change request are on (the field is true when they are OFF, so
    /// the default is on).
    #[prost(bool, tag = "20")]
    pub previews_off: bool,
    /// Days a preview lives.
    #[prost(uint32, tag = "21")]
    pub preview_ttl_days: u32,
    /// Org name when created (for URLs).
    #[prost(string, tag = "22")]
    pub org_name: String,
    /// Repo name when created (for URLs).
    #[prost(string, tag = "23")]
    pub repo_name: String,
}

/// One build or deployment.
#[derive(Clone, PartialEq, prost::Message)]
pub struct DeploymentRec {
    /// Deployment id (ULID).
    #[prost(string, tag = "1")]
    pub id: String,
    /// Repo id.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// [`bee_proto::v1::PagesDeploymentState`].
    #[prost(int32, tag = "3")]
    pub state: i32,
    /// [`bee_proto::v1::PagesBuildType`].
    #[prost(int32, tag = "4")]
    pub build_type: i32,
    /// Source ref.
    #[prost(string, tag = "5")]
    pub git_ref: String,
    /// Source changeset.
    #[prost(bytes = "vec", tag = "6")]
    pub changeset: Vec<u8>,
    /// Workflow artifact.
    #[prost(string, tag = "7")]
    pub artifact_id: String,
    /// Workflow run.
    #[prost(string, tag = "8")]
    pub run_id: String,
    /// Manifest id (content hash); empty until built.
    #[prost(string, tag = "9")]
    pub manifest_id: String,
    /// Public URL.
    #[prost(string, tag = "10")]
    pub page_url: String,
    /// Preview URL for a preview deployment.
    #[prost(string, tag = "11")]
    pub preview_url: String,
    /// Failure.
    #[prost(string, tag = "12")]
    pub error_message: String,
    /// Build duration.
    #[prost(uint64, tag = "13")]
    pub duration_ms: u64,
    /// Creator principal id.
    #[prost(string, tag = "14")]
    pub creator: String,
    /// Creator kind.
    #[prost(int32, tag = "15")]
    pub creator_kind: i32,
    /// Creation time.
    #[prost(uint64, tag = "16")]
    pub created_ms: u64,
    /// Last change.
    #[prost(uint64, tag = "17")]
    pub updated_ms: u64,
    /// A preview deployment (does not replace the live site).
    #[prost(bool, tag = "18")]
    pub preview: bool,
    /// Change request number of a per-CR preview, 0 otherwise.
    #[prost(uint64, tag = "19")]
    pub cr_number: u64,
    /// Preview expiry, 0 for live deployments.
    #[prost(uint64, tag = "20")]
    pub expires_ms: u64,
    /// Preview host.
    #[prost(string, tag = "21")]
    pub host: String,
    /// Environment name.
    #[prost(string, tag = "22")]
    pub environment: String,
}

/// A file of a deployment.
#[derive(Clone, PartialEq, prost::Message)]
pub struct EntryRec {
    /// Path from the site root, no leading slash.
    #[prost(string, tag = "1")]
    pub path: String,
    /// SHA-256 hex of the content (the blob id).
    #[prost(string, tag = "2")]
    pub blob: String,
    /// Size in bytes.
    #[prost(uint64, tag = "3")]
    pub size: u64,
    /// Content type.
    #[prost(string, tag = "4")]
    pub content_type: String,
}

/// A redirect rule from the `_redirects` file.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RedirectRec {
    /// Source pattern (`/old`, `/blog/*`).
    #[prost(string, tag = "1")]
    pub from: String,
    /// Target (`/new`, `/posts/:splat`, absolute URL).
    #[prost(string, tag = "2")]
    pub to: String,
    /// Status (301, 302, 307, 308, or 200 for a rewrite).
    #[prost(uint32, tag = "3")]
    pub status: u32,
    /// Apply even when a file exists at the path.
    #[prost(bool, tag = "4")]
    pub force: bool,
}

/// A content-addressed manifest: the files of a site at one point.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ManifestRec {
    /// Manifest id: SHA-256 over the canonical file list.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Files sorted by path.
    #[prost(message, repeated, tag = "2")]
    pub entries: Vec<EntryRec>,
    /// Redirect rules, in file order.
    #[prost(message, repeated, tag = "3")]
    pub redirects: Vec<RedirectRec>,
    /// Total size.
    #[prost(uint64, tag = "4")]
    pub total_bytes: u64,
}

/// What a host name serves.
#[derive(Clone, PartialEq, prost::Message)]
pub struct HostRec {
    /// Repo id.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// `custom`, `preview` or `default`.
    #[prost(string, tag = "2")]
    pub kind: String,
    /// Deployment for a preview host.
    #[prost(string, tag = "3")]
    pub deployment_id: String,
}

/// An org's claim on a domain.
#[derive(Clone, PartialEq, prost::Message)]
pub struct VerificationRec {
    /// Id (ULID).
    #[prost(string, tag = "1")]
    pub id: String,
    /// Org id.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Org name when created.
    #[prost(string, tag = "3")]
    pub org_name: String,
    /// Domain, lower case.
    #[prost(string, tag = "4")]
    pub domain: String,
    /// [`bee_proto::v1::PagesDomainVerificationState`].
    #[prost(int32, tag = "5")]
    pub state: i32,
    /// TXT record value to publish.
    #[prost(string, tag = "6")]
    pub txt_value: String,
    /// Verification time.
    #[prost(uint64, tag = "7")]
    pub verified_ms: u64,
    /// Creation time.
    #[prost(uint64, tag = "8")]
    pub created_ms: u64,
}

/// Per-org Pages settings.
#[derive(Clone, PartialEq, prost::Message)]
pub struct OrgSettingsRec {
    /// The org forbids public sites.
    #[prost(bool, tag = "1")]
    pub forbid_public: bool,
}
