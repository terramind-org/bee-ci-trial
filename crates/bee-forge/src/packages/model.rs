//! Stored records of the packages surface. They are crate-private protobuf messages (the
//! forge convention for bodies that are not a public `bee.v1` message); the public shapes
//! are built from them by [`super::Packages`].

use bee_proto::v1::{PackageEcosystem, PackageOwnerKind, PackageVisibility};

use crate::error::{ForgeError, Result};

/// The registry a package belongs to. `Cargo` is a Beehive addition beyond
/// [`PackageEcosystem`]; it travels as the enum number 6 (see the API gaps in
/// `docs/spec/packages-pages.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Eco {
    /// OCI images and artifacts.
    Container,
    /// npm.
    Npm,
    /// Maven and Gradle.
    Maven,
    /// NuGet.
    Nuget,
    /// RubyGems.
    Rubygems,
    /// Cargo crates (sparse registry).
    Cargo,
}

impl Eco {
    /// Every ecosystem.
    pub const ALL: [Eco; 6] = [
        Eco::Container,
        Eco::Npm,
        Eco::Maven,
        Eco::Nuget,
        Eco::Rubygems,
        Eco::Cargo,
    ];

    /// The wire number (the proto enum, extended with Cargo = 6).
    pub fn number(self) -> i32 {
        match self {
            Eco::Container => PackageEcosystem::Container as i32,
            Eco::Npm => PackageEcosystem::Npm as i32,
            Eco::Maven => PackageEcosystem::Maven as i32,
            Eco::Nuget => PackageEcosystem::Nuget as i32,
            Eco::Rubygems => PackageEcosystem::Rubygems as i32,
            Eco::Cargo => 6,
        }
    }

    /// Parses a wire number; `UNSPECIFIED` and unknown numbers are `INVALID_ARGUMENT`.
    pub fn from_number(n: i32) -> Result<Eco> {
        Eco::ALL
            .into_iter()
            .find(|e| e.number() == n)
            .ok_or_else(|| ForgeError::invalid("bad_ecosystem", "`ecosystem` is required"))
    }

    /// Like [`Eco::from_number`] but `UNSPECIFIED` is `None` (a filter).
    pub fn filter(n: i32) -> Result<Option<Eco>> {
        if n == PackageEcosystem::Unspecified as i32 {
            Ok(None)
        } else {
            Eco::from_number(n).map(Some)
        }
    }

    /// Short lower-case name, used in keys and URLs.
    pub fn slug(self) -> &'static str {
        match self {
            Eco::Container => "container",
            Eco::Npm => "npm",
            Eco::Maven => "maven",
            Eco::Nuget => "nuget",
            Eco::Rubygems => "rubygems",
            Eco::Cargo => "cargo",
        }
    }
}

/// A package.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PackageRec {
    /// Package id (ULID).
    #[prost(string, tag = "1")]
    pub id: String,
    /// Owning org or user id.
    #[prost(string, tag = "2")]
    pub owner_id: String,
    /// [`PackageOwnerKind`].
    #[prost(int32, tag = "3")]
    pub owner_kind: i32,
    /// [`Eco::number`].
    #[prost(int32, tag = "4")]
    pub ecosystem: i32,
    /// Name as first published.
    #[prost(string, tag = "5")]
    pub name: String,
    /// [`PackageVisibility`].
    #[prost(int32, tag = "6")]
    pub visibility: i32,
    /// Linked repo id.
    #[prost(string, tag = "7")]
    pub repo_id: String,
    /// Access follows the linked repo.
    #[prost(bool, tag = "8")]
    pub inherit_access: bool,
    /// Description.
    #[prost(string, tag = "9")]
    pub description: String,
    /// Creation time.
    #[prost(uint64, tag = "10")]
    pub created_ms: u64,
    /// Last change.
    #[prost(uint64, tag = "11")]
    pub updated_ms: u64,
    /// Deletion time, 0 while live.
    #[prost(uint64, tag = "12")]
    pub deleted_ms: u64,
    /// Principal that created it.
    #[prost(string, tag = "13")]
    pub creator: String,
    /// Live version count.
    #[prost(uint32, tag = "14")]
    pub version_count: u32,
}

impl PackageRec {
    /// The ecosystem.
    pub fn eco(&self) -> Result<Eco> {
        Eco::from_number(self.ecosystem)
    }

    /// The visibility.
    pub fn vis(&self) -> PackageVisibility {
        PackageVisibility::try_from(self.visibility).unwrap_or(PackageVisibility::Private)
    }

    /// The owner kind.
    pub fn kind(&self) -> PackageOwnerKind {
        PackageOwnerKind::try_from(self.owner_kind).unwrap_or(PackageOwnerKind::Org)
    }
}

/// A platform of a multi-platform image.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PlatformRec {
    /// Operating system.
    #[prost(string, tag = "1")]
    pub os: String,
    /// Architecture.
    #[prost(string, tag = "2")]
    pub arch: String,
    /// Variant.
    #[prost(string, tag = "3")]
    pub variant: String,
    /// Manifest digest.
    #[prost(string, tag = "4")]
    pub digest: String,
}

/// A package version.
#[derive(Clone, PartialEq, prost::Message)]
pub struct VersionRec {
    /// Version id (ULID).
    #[prost(string, tag = "1")]
    pub id: String,
    /// Package id.
    #[prost(string, tag = "2")]
    pub package_id: String,
    /// Version string as published (containers: the manifest digest).
    #[prost(string, tag = "3")]
    pub version: String,
    /// Containers: manifest or index digest.
    #[prost(string, tag = "4")]
    pub digest: String,
    /// Total size of the files.
    #[prost(uint64, tag = "6")]
    pub size_bytes: u64,
    /// Publisher principal id.
    #[prost(string, tag = "7")]
    pub publisher_id: String,
    /// Publisher principal kind.
    #[prost(int32, tag = "8")]
    pub publisher_kind: i32,
    /// Source changeset id.
    #[prost(bytes = "vec", tag = "9")]
    pub changeset: Vec<u8>,
    /// Workflow run id.
    #[prost(string, tag = "10")]
    pub run_id: String,
    /// Provenance recorded.
    #[prost(bool, tag = "11")]
    pub has_provenance: bool,
    /// Creation time.
    #[prost(uint64, tag = "12")]
    pub created_ms: u64,
    /// Last change.
    #[prost(uint64, tag = "13")]
    pub updated_ms: u64,
    /// Deletion time, 0 while live.
    #[prost(uint64, tag = "14")]
    pub deleted_ms: u64,
    /// Ecosystem metadata JSON.
    #[prost(string, tag = "15")]
    pub metadata_json: String,
    /// Platforms of an image index.
    #[prost(message, repeated, tag = "16")]
    pub platforms: Vec<PlatformRec>,
    /// npm deprecation message; empty when not deprecated.
    #[prost(string, tag = "17")]
    pub deprecated: String,
    /// Cargo and RubyGems yank flag.
    #[prost(bool, tag = "18")]
    pub yanked: bool,
    /// Containers: manifest media type.
    #[prost(string, tag = "19")]
    pub media_type: String,
    /// Containers: digest of the manifest this one refers to (OCI `subject`).
    #[prost(string, tag = "20")]
    pub subject: String,
    /// Containers: `artifactType` or config media type, for referrers filtering.
    #[prost(string, tag = "21")]
    pub artifact_type: String,
    /// Owning org or user id (lets the purge find the owner without the package).
    #[prost(string, tag = "22")]
    pub owner_id: String,
}

/// One file of a version.
#[derive(Clone, PartialEq, prost::Message)]
pub struct FileRec {
    /// File name within the version.
    #[prost(string, tag = "1")]
    pub name: String,
    /// Size in bytes.
    #[prost(uint64, tag = "2")]
    pub size: u64,
    /// `sha256:<hex>`.
    #[prost(string, tag = "3")]
    pub digest: String,
    /// Media type.
    #[prost(string, tag = "4")]
    pub media_type: String,
    /// Blob store key.
    #[prost(string, tag = "5")]
    pub blob_key: String,
    /// SHA-1 hex (npm `shasum`, Maven `.sha1`).
    #[prost(string, tag = "6")]
    pub sha1: String,
    /// MD5 hex (Maven `.md5`, gem compact index).
    #[prost(string, tag = "7")]
    pub md5: String,
    /// SHA-512 base64 (npm `integrity`).
    #[prost(string, tag = "8")]
    pub sha512_b64: String,
    /// Upload time.
    #[prost(uint64, tag = "9")]
    pub created_ms: u64,
}

/// An OCI blob belonging to a package.
#[derive(Clone, PartialEq, prost::Message)]
pub struct BlobRec {
    /// `sha256:<hex>`.
    #[prost(string, tag = "1")]
    pub digest: String,
    /// Size.
    #[prost(uint64, tag = "2")]
    pub size: u64,
    /// Blob store key.
    #[prost(string, tag = "3")]
    pub blob_key: String,
    /// Upload time.
    #[prost(uint64, tag = "4")]
    pub created_ms: u64,
}

/// A tag (container tag, npm dist-tag).
#[derive(Clone, PartialEq, prost::Message)]
pub struct TagRec {
    /// Tag name.
    #[prost(string, tag = "1")]
    pub tag: String,
    /// The version it points at.
    #[prost(string, tag = "2")]
    pub version_id: String,
    /// Last move.
    #[prost(uint64, tag = "3")]
    pub updated_ms: u64,
}

/// A file announced by a native publish.
#[derive(Clone, PartialEq, prost::Message)]
pub struct NativeFile {
    /// Name within the version.
    #[prost(string, tag = "1")]
    pub name: String,
    /// Declared size.
    #[prost(uint64, tag = "2")]
    pub size: u64,
    /// Declared digest.
    #[prost(string, tag = "3")]
    pub digest: String,
    /// Media type.
    #[prost(string, tag = "4")]
    pub media_type: String,
    /// Staging key the file is uploaded to.
    #[prost(string, tag = "5")]
    pub key: String,
}

/// An upload session: an OCI blob upload in progress or a native publish.
#[derive(Clone, PartialEq, prost::Message)]
pub struct UploadRec {
    /// Session id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Package id.
    #[prost(string, tag = "2")]
    pub package_id: String,
    /// Owner id.
    #[prost(string, tag = "3")]
    pub owner_id: String,
    /// Start time.
    #[prost(uint64, tag = "4")]
    pub started_ms: u64,
    /// Expiry.
    #[prost(uint64, tag = "5")]
    pub expires_ms: u64,
    /// OCI: bytes received so far.
    #[prost(uint64, tag = "6")]
    pub size: u64,
    /// OCI: chunks received so far.
    #[prost(uint32, tag = "7")]
    pub chunks: u32,
    /// Principal that started it.
    #[prost(string, tag = "8")]
    pub principal: String,
    /// A native publish.
    #[prost(bool, tag = "10")]
    pub native: bool,
    /// Native: version.
    #[prost(string, tag = "13")]
    pub version: String,
    /// Native: announced files.
    #[prost(message, repeated, tag = "14")]
    pub files: Vec<NativeFile>,
    /// Native: ecosystem metadata JSON.
    #[prost(string, tag = "15")]
    pub metadata_json: String,
}

/// Retention policy of a package or an owner's default.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RetentionRec {
    /// Keep at most this many live versions (0 = unlimited); the newest are kept.
    #[prost(uint32, tag = "1")]
    pub keep_last: u32,
    /// Delete untagged versions older than this many days (0 = never).
    #[prost(uint32, tag = "2")]
    pub untagged_days: u32,
    /// Delete versions older than this many days, tagged or not (0 = never).
    #[prost(uint32, tag = "3")]
    pub older_than_days: u32,
}

/// Per-owner package settings.
#[derive(Clone, PartialEq, prost::Message)]
pub struct OwnerSettingsRec {
    /// The owner forbids public packages (`POLICY_DENIED` on publishing or making public).
    #[prost(bool, tag = "1")]
    pub forbid_public: bool,
    /// Storage quota across the owner's packages, 0 = unlimited.
    #[prost(uint64, tag = "2")]
    pub storage_quota_bytes: u64,
}
