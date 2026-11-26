//! Server configuration (PRODUCT §4.22): one TOML file, strictly validated.
//!
//! - Unknown keys are errors at every level (`deny_unknown_fields`).
//! - A string value that is exactly `$NAME` or `${NAME}` is replaced by the environment
//!   variable `NAME` before parsing; an unset variable is an error. `$$` at the start escapes
//!   a literal `$`.
//! - Every key has a documented default (see `docs/spec/server.md`); an empty file is a valid
//!   configuration that serves SQLite and disk storage under `./bee-data`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A configuration problem: which key, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "config: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The whole configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Listener, identity of this server and data directory.
    pub server: ServerSection,
    /// Metadata store.
    pub metadata: MetadataSection,
    /// Blob store.
    pub blobs: BlobSection,
    /// Token signing and service identities.
    pub auth: AuthSection,
    /// Org-level policy defaults.
    pub policy: PolicySection,
    /// Land coalescer.
    pub land: LandSection,
    /// Deadlines and request limits.
    pub limits: LimitsSection,
    /// Admission control and quotas.
    pub admission: AdmissionSection,
    /// Secret scanning push protection.
    pub push_protection: PushProtectionSection,
    /// Git history imports.
    pub import: ImportSection,
    /// Redaction lifecycle.
    pub redaction: RedactionSection,
    /// Structured logs.
    pub logging: LoggingSection,
    /// The Beehive forge.
    pub forge: ForgeSection,
    /// Code search index (`docs/spec/operations.md` §2).
    pub search: SearchSection,
    /// Backups and restores (`docs/spec/operations.md` §4).
    pub backup: BackupSection,
    /// Audit log sinks and exports (`docs/spec/operations.md` §5).
    pub audit: AuditSection,
    /// Operator surface (`docs/spec/operations.md` §3).
    pub ops: OpsSection,
    /// Human login: GitHub device flow and token exchange, external OIDC, Beehive as an
    /// OIDC provider (SPEC §9.1).
    pub login: LoginSection,
    /// TLS on the main listener, with optional client certificates (mutual TLS).
    pub tls: TlsSection,
    /// OpenTelemetry trace export (`crate::otel`).
    pub tracing: TracingSection,
}

/// `[tracing]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TracingSection {
    /// OTLP/HTTP collector base URL (spans go to `<url>/v1/traces`); empty = no export.
    pub otlp_endpoint: String,
    /// How often queued spans are exported, in milliseconds.
    pub export_interval_ms: u64,
    /// Share of new traces (requests without `traceparent`) that are sampled, 0 to 1.
    pub sample_ratio: f64,
    /// `service.name` of exported spans.
    pub service_name: String,
}

impl Default for TracingSection {
    fn default() -> Self {
        TracingSection {
            otlp_endpoint: String::new(),
            export_interval_ms: 1000,
            sample_ratio: 1.0,
            service_name: "bee-server".into(),
        }
    }
}

/// `[login]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LoginSection {
    /// GitHub web base for the device flow (`<base>/login/device/code`,
    /// `<base>/login/oauth/access_token`); empty = `https://github.com`.
    pub github_url: String,
    /// GitHub REST base (`/user`, `/user/teams`); empty = `https://api.github.com`.
    pub github_api_url: String,
    /// Client id of the GitHub OAuth app humans log in through; empty disables the GitHub
    /// device flow (token exchange needs no app).
    pub github_client_id: String,
    /// Scopes the device flow asks for (`read:org` lets team sync read team membership).
    pub github_scopes: String,
    /// Replace the principal's external groups with its GitHub teams (`<org>/<team>`) at
    /// every GitHub login.
    pub github_team_sync: bool,
    /// Only teams of these GitHub orgs are synced (empty: every org).
    pub github_team_orgs: Vec<String>,
    /// Org new GitHub or OIDC principals belong to (empty: none).
    pub default_org: String,
    /// Issuer of an external OIDC identity provider whose id_tokens `Login` accepts; empty
    /// disables external OIDC.
    pub oidc_issuer: String,
    /// The audience (`aud`) id_tokens must carry: this server's client id at the IdP.
    pub oidc_client_id: String,
    /// JWKS URL; empty = `jwks_uri` from `<issuer>/.well-known/openid-configuration`.
    pub oidc_jwks_url: String,
    /// The id_token claim holding the user's groups.
    pub oidc_groups_claim: String,
    /// Seconds of clock skew allowed on `exp`, `nbf` and `iat`.
    pub oidc_leeway_secs: u64,
    /// Lifetime of id_tokens this server issues as an OIDC provider, in seconds.
    pub id_token_ttl_secs: u64,
}

impl Default for LoginSection {
    fn default() -> Self {
        LoginSection {
            github_url: String::new(),
            github_api_url: String::new(),
            github_client_id: String::new(),
            github_scopes: "read:user read:org".into(),
            github_team_sync: true,
            github_team_orgs: Vec::new(),
            default_org: String::new(),
            oidc_issuer: String::new(),
            oidc_client_id: String::new(),
            oidc_jwks_url: String::new(),
            oidc_groups_claim: "groups".into(),
            oidc_leeway_secs: 60,
            id_token_ttl_secs: 3600,
        }
    }
}

/// `[tls]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TlsSection {
    /// PEM certificate chain of the main listener; with `key_file` the listener speaks TLS
    /// only (HTTP/1.1 and HTTP/2 by ALPN).
    pub cert_file: String,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    pub key_file: String,
    /// PEM CA certificates that sign client certificates; set, the listener asks for a
    /// client certificate (mutual TLS).
    pub client_ca_file: String,
    /// Refuse connections without a valid client certificate (otherwise optional).
    pub require_client_cert: bool,
    /// Client certificates that act as principals: a certificate whose subject common name
    /// or SAN DNS name is `subject` is the service or human `principal` (a request with such
    /// a certificate and no bearer token authenticates as it).
    pub client_principals: Vec<TlsPrincipal>,
}

/// One `[[tls.client_principals]]` entry.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TlsPrincipal {
    /// Certificate subject common name or SAN DNS name.
    pub subject: String,
    /// The principal id it maps to.
    pub principal: String,
}

/// `[search]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SearchSection {
    /// Keep the code index and serve code search.
    pub enabled: bool,
    /// Index directory; empty = `<data_dir>/search`.
    pub index_dir: String,
    /// How often the indexer looks for moved refs, in milliseconds.
    pub poll_ms: u64,
    /// Ref names indexed per repo (exact names).
    pub refs: Vec<String>,
    /// Curated topics shown by `ListFeaturedTopics` (topic names).
    pub featured_topics: Vec<String>,
    /// Curated collections served by `ListTopicCollections` and `GetTopicCollection`.
    pub collections: Vec<CollectionEntry>,
}

impl Default for SearchSection {
    fn default() -> Self {
        SearchSection {
            enabled: true,
            index_dir: String::new(),
            poll_ms: 500,
            refs: vec!["main".into()],
            featured_topics: Vec::new(),
            collections: Vec::new(),
        }
    }
}

/// One `[[search.collections]]` entry.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct CollectionEntry {
    /// URL slug.
    pub slug: String,
    /// Title.
    pub display_name: String,
    /// Markdown description.
    pub description: String,
    /// Image URL.
    pub image_url: String,
    /// Items: `org/repo`, `@login`, or an `http(s)://` URL.
    pub items: Vec<String>,
}

/// `[backup]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct BackupSection {
    /// Where backups go: empty or `dir:<path>` = a directory (default `<data_dir>/backups`),
    /// `s3` = the S3-compatible bucket of `[blobs]` under `s3_prefix`.
    pub target: String,
    /// Key prefix of backups in the bucket (`target = "s3"`).
    pub s3_prefix: String,
    /// Where restores write their data directories (`<restore_root>/<target_cell>`); empty =
    /// `<data_dir>/restores`.
    pub restore_root: String,
    /// Days a backup is kept (at most 30, PRODUCT §4.3).
    pub retention_days: u64,
}

impl Default for BackupSection {
    fn default() -> Self {
        BackupSection {
            target: String::new(),
            s3_prefix: "bee-backups/".into(),
            restore_root: String::new(),
            retention_days: 30,
        }
    }
}

/// `[audit]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AuditSection {
    /// Directories `file://` sinks may write under (none = file sinks are refused).
    pub file_roots: Vec<String>,
    /// How often sinks deliver new entries, in milliseconds.
    pub sink_poll_ms: u64,
    /// Entries per delivery.
    pub sink_batch: usize,
    /// Per-request timeout of HTTP deliveries, in milliseconds.
    pub sink_timeout_ms: u64,
    /// Delay before the first retry of a failed delivery, in milliseconds (doubles per
    /// failure).
    pub sink_retry_base_ms: u64,
    /// Upper bound of the retry delay, in milliseconds.
    pub sink_retry_max_ms: u64,
    /// Consecutive failures after which a sink reports `FAILING`.
    pub sink_failing_after: u32,
}

impl Default for AuditSection {
    fn default() -> Self {
        AuditSection {
            file_roots: Vec::new(),
            sink_poll_ms: 1000,
            sink_batch: 200,
            sink_timeout_ms: 10_000,
            sink_retry_base_ms: 1000,
            sink_retry_max_ms: 300_000,
            sink_failing_after: 3,
        }
    }
}

/// `[ops]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct OpsSection {
    /// Principal ids allowed on the server-wide operator RPCs (besides `forge.site_admins`).
    /// Org admins are not operators; dev mode lists the dev user here at startup.
    pub operators: Vec<String>,
    /// Interval of the metrics sampler behind `QueryMetricsRange`, in milliseconds (0 =
    /// 10 s).
    pub metrics_sample_ms: u64,
}

/// The edition of the forge a server runs as (`[server] edition`).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    /// Hosted multi-tenant service with sign-up and billing.
    Cloud,
    /// Self-hosted instance administered by its operators.
    #[default]
    Enterprise,
}

impl Edition {
    /// The value `ServerInfo.edition` carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::Cloud => "cloud",
            Edition::Enterprise => "enterprise",
        }
    }
}

/// `[server]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    /// Address to listen on (gRPC, HTTP/JSON, MCP, health and metrics share it).
    pub listen: String,
    /// Public base URL (signed URLs, `connect_base_url`); empty = `http://<bound address>`.
    pub public_url: String,
    /// URL other server processes use to forward lands to this one when it owns a
    /// coalescer; empty = `public_url`.
    pub advertise_url: String,
    /// Data directory for SQLite files, disk blobs and keys.
    pub data_dir: PathBuf,
    /// Cell name reported by `GetServerInfo`.
    pub cell: String,
    /// Region reported by `GetServerInfo`.
    pub region: String,
    /// Which edition of the forge this server is: hosted `cloud` or self-hosted `enterprise`.
    /// Reported by `GetServerInfo`; the web app reads it to choose its surfaces.
    pub edition: Edition,
    /// Take the client address from the first `X-Forwarded-For` entry (behind a trusted
    /// proxy); otherwise the TCP peer address is used and the header is replaced with it.
    pub trust_forwarded_for: bool,
    /// Share changesets and graph nodes read by one request with the next ones on the same
    /// repo (both are immutable under their id); off, each request reads its own.
    pub share_history_rows: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection {
            listen: "127.0.0.1:7420".into(),
            public_url: String::new(),
            advertise_url: String::new(),
            data_dir: PathBuf::from("./bee-data"),
            cell: "local".into(),
            region: "local".into(),
            edition: Edition::Enterprise,
            trust_forwarded_for: false,
            share_history_rows: true,
        }
    }
}

/// `[metadata]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct MetadataSection {
    /// `sqlite` or `postgres`.
    pub backend: String,
    /// SQLite database file; empty = `<data_dir>/meta.db`.
    pub sqlite_path: String,
    /// Postgres connection string (required for `postgres`).
    pub postgres_url: String,
    /// Postgres read replica connection string.
    pub postgres_replica_url: String,
    /// Postgres schema; empty = the connection default.
    pub postgres_schema: String,
    /// Postgres connections per server.
    pub max_connections: usize,
    /// Statement deadline in milliseconds.
    pub query_timeout_ms: u64,
    /// Replica wait for `min_version` reads (SPEC §6.3 `read_wait_ms`).
    pub read_wait_ms: u64,
    /// Staleness bound of ref reads while the metadata primary is unavailable (SPEC §15.1):
    /// a ref this process read at most this long ago is returned marked `stale`; 0 turns
    /// stale reads off (the read fails with `UNAVAILABLE`).
    pub stale_read_max_ms: u64,
    /// Server-owned tables (principals, audit, leases, imports) in SQLite; empty =
    /// `<data_dir>/server.db`. With `postgres` they live in the same database.
    pub server_sqlite_path: String,
}

impl Default for MetadataSection {
    fn default() -> Self {
        MetadataSection {
            backend: "sqlite".into(),
            sqlite_path: String::new(),
            postgres_url: String::new(),
            postgres_replica_url: String::new(),
            postgres_schema: String::new(),
            max_connections: 16,
            query_timeout_ms: 5_000,
            read_wait_ms: 50,
            stale_read_max_ms: 300_000,
            server_sqlite_path: String::new(),
        }
    }
}

/// `[blobs]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct BlobSection {
    /// `disk` or `s3`.
    pub backend: String,
    /// Disk root; empty = `<data_dir>/blobs`.
    pub disk_root: String,
    /// zstd level for disk blobs.
    pub zstd_level: i32,
    /// S3 bucket.
    pub s3_bucket: String,
    /// S3 region.
    pub s3_region: String,
    /// S3 endpoint for MinIO/R2 (path-style addressing); empty = AWS.
    pub s3_endpoint: String,
    /// S3 access key id.
    pub s3_access_key_id: String,
    /// S3 secret access key.
    pub s3_secret_access_key: String,
    /// Key prefix inside the bucket.
    pub s3_prefix: String,
    /// Serve large objects by signed URL when clients allow it (SPEC §10.5).
    pub signed_urls: bool,
    /// Lifetime of a signed URL in seconds (`signed_url_ttl`).
    pub signed_url_ttl_secs: u64,
    /// HMAC key for local signed URLs (hex, at least 32 bytes); empty = generated and kept
    /// in `<data_dir>/url.key`.
    pub url_key_hex: String,
}

impl Default for BlobSection {
    fn default() -> Self {
        BlobSection {
            backend: "disk".into(),
            disk_root: String::new(),
            zstd_level: 3,
            s3_bucket: String::new(),
            s3_region: "us-east-1".into(),
            s3_endpoint: String::new(),
            s3_access_key_id: String::new(),
            s3_secret_access_key: String::new(),
            s3_prefix: String::new(),
            signed_urls: true,
            signed_url_ttl_secs: 60,
            url_key_hex: String::new(),
        }
    }
}

/// `[auth]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    /// Ed25519 signing key secret (64 hex digits); empty = read or create
    /// `<data_dir>/signing.key`.
    pub signing_key_hex: String,
    /// ES256 (P-256) key of the OpenID provider, Actions OIDC tokens and attestations, as hex
    /// of a PKCS#8 document; empty = read or create `<data_dir>/oidc-signing-key.p8`. Replicas
    /// sharing one store must all set the same key, or the JWKS one serves does not verify
    /// the tokens another signs.
    pub oidc_key_pkcs8_hex: String,
    /// Key id written into tokens.
    pub signing_key_id: u32,
    /// Lifetime of tokens minted without an expiry, in seconds (SPEC §9.2: 12 h).
    pub token_lifetime_secs: u64,
    /// How often revocations made by other processes are picked up, in milliseconds (the
    /// revocation propagation bound).
    pub revocation_refresh_ms: u64,
    /// Service identities that log in with a secret.
    pub services: Vec<ServiceEntry>,
}

impl Default for AuthSection {
    fn default() -> Self {
        AuthSection {
            signing_key_hex: String::new(),
            oidc_key_pkcs8_hex: String::new(),
            signing_key_id: 1,
            token_lifetime_secs: 12 * 3600,
            revocation_refresh_ms: 1_000,
            services: Vec::new(),
        }
    }
}

/// One `[[auth.services]]` entry.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceEntry {
    /// Login name.
    pub name: String,
    /// Display name of the service principal.
    #[serde(default)]
    pub display: String,
    /// Org the service belongs to.
    #[serde(default)]
    pub org: String,
    /// SHA-256 of the secret, 64 hex digits (never the secret itself).
    pub secret_sha256: String,
}

/// `[policy]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PolicySection {
    /// Org default policy file (SPEC §9.4 format) applied to every org; empty = the built-in
    /// default (everyone reads and writes, `main` and `release/*` protected, org admins
    /// administer).
    pub org_default_file: String,
}

/// `[land]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LandSection {
    /// Most queued lands one coalescer publishes in one CAS.
    pub batch_limit: usize,
    /// Attempts per land before `LAND_CONTENTION`.
    pub attempt_budget: u32,
    /// Lifetime of a coalescer owner lease in milliseconds (renewed at a third of it).
    pub owner_lease_ms: u64,
    /// After a land, derive the blame of the files it changed in the background.
    pub warm_blame: bool,
    /// After a land, derive the git ids of the new tip in the background (SPEC §17.4), so
    /// the first git fetch of it reads stored mappings.
    pub warm_git: bool,
    /// After a land, build in the background the pack a stock `git clone` of the repo asks
    /// for and keep it in the gateway's pack cache, so the first clone of the new tip is a
    /// cache hit.
    pub warm_pack: bool,
}

impl Default for LandSection {
    fn default() -> Self {
        LandSection {
            batch_limit: 512,
            attempt_budget: 16,
            owner_lease_ms: 6_000,
            warm_blame: true,
            warm_git: true,
            warm_pack: true,
        }
    }
}

/// `[limits]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    /// Deadline of a request that sets none, in milliseconds.
    pub default_deadline_ms: u64,
    /// Deadline of one forge-backed lookup, in milliseconds (100 to 60000).
    pub forge_lookup_deadline_ms: u64,
    /// Longest deadline a client may ask for.
    pub max_deadline_ms: u64,
    /// Ids per `GetObjects`/`HasObjects` request.
    pub max_batch_ids: u32,
    /// Largest blob accepted, in bytes.
    pub max_blob_bytes: u64,
    /// Largest message on the HTTP/JSON surface, in bytes.
    pub max_message_bytes: usize,
}

impl Default for LimitsSection {
    fn default() -> Self {
        LimitsSection {
            default_deadline_ms: 30_000,
            forge_lookup_deadline_ms: 3_000,
            max_deadline_ms: 300_000,
            max_batch_ids: 1_000,
            max_blob_bytes: 100 << 20,
            max_message_bytes: 64 << 20,
        }
    }
}

/// `[admission]` (SPEC §15.4).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AdmissionSection {
    /// Requests in flight at once, all tiers.
    pub max_concurrent: usize,
    /// Slots only `interactive` and `land` may use.
    pub reserved_interactive_land: usize,
    /// Most `bulk` requests in flight.
    pub max_bulk: usize,
    /// Most `anonymous` requests in flight.
    pub max_anonymous: usize,
    /// How long a request may wait for a slot before it is shed, in milliseconds.
    pub queue_wait_ms: u64,
    /// Requests per second per principal (token bucket, burst twice the rate); 0 = no limit.
    pub per_principal_rps: u64,
    /// Requests per second shared by everything under one delegation root; 0 = no limit.
    pub per_root_rps: u64,
    /// Changesets per delegation root (quota, visible through `WhoAmI`); 0 = no limit.
    pub root_max_changesets: u64,
    /// Landed changes per delegation root; 0 = no limit.
    pub root_max_landed: u64,
    /// Uploaded bytes per delegation root; 0 = no limit.
    pub root_max_upload_bytes: u64,
}

impl Default for AdmissionSection {
    fn default() -> Self {
        AdmissionSection {
            max_concurrent: 8_192,
            reserved_interactive_land: 2_048,
            max_bulk: 256,
            max_anonymous: 64,
            queue_wait_ms: 5_000,
            per_principal_rps: 5_000,
            per_root_rps: 20_000,
            root_max_changesets: 0,
            root_max_landed: 0,
            root_max_upload_bytes: 0,
        }
    }
}

/// `[push_protection]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PushProtectionSection {
    /// Scan blobs added by CreateChangeset for secrets.
    pub enabled: bool,
    /// Who may bypass with a reason: `writers` (anyone who may write the path), `admins`
    /// (principals holding `manage_policy`) or `none`.
    pub bypass: String,
}

impl Default for PushProtectionSection {
    fn default() -> Self {
        PushProtectionSection {
            enabled: true,
            bypass: "writers".into(),
        }
    }
}

/// `[import]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ImportSection {
    /// Directories under which `StartImport` may read local git repositories; empty
    /// disables local-path imports (dev mode allows any path).
    pub allowed_roots: Vec<String>,
    /// GitHub API base for GitHub imports; empty = `https://api.github.com` (set it for
    /// GitHub Enterprise Server: REST at the base, GraphQL at `<base>/graphql`).
    pub github_url: String,
    /// GitHub App id used for imports given a `github_installation_id`.
    pub github_app_id: String,
    /// PEM private key file of that GitHub App.
    pub github_app_private_key_file: String,
    /// Secret of the `POST /webhooks/github` live-sync receiver (`X-Hub-Signature-256`).
    pub github_webhook_secret: String,
    /// Polling interval of live GitHub imports, in seconds (0 = webhooks only).
    pub github_sync_interval_secs: u64,
    /// GitHub OAuth app that verifies `ClaimIdentity` codes.
    pub github_oauth_client_id: String,
    /// Its client secret.
    pub github_oauth_client_secret: String,
}

impl Default for ImportSection {
    fn default() -> Self {
        ImportSection {
            allowed_roots: Vec::new(),
            github_url: String::new(),
            github_app_id: String::new(),
            github_app_private_key_file: String::new(),
            github_webhook_secret: String::new(),
            github_sync_interval_secs: 300,
            github_oauth_client_id: String::new(),
            github_oauth_client_secret: String::new(),
        }
    }
}

/// `[redaction]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RedactionSection {
    /// Days a redacted object stays quarantined (unredactable) before it is purged.
    pub quarantine_days: u64,
    /// How often scheduled purges run, in seconds.
    pub purge_interval_secs: u64,
}

impl Default for RedactionSection {
    fn default() -> Self {
        RedactionSection {
            quarantine_days: 7,
            purge_interval_secs: 60,
        }
    }
}

/// `[logging]`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingSection {
    /// `error`, `warn`, `info` or `debug`.
    pub level: String,
    /// Log one line per request.
    pub requests: bool,
}

impl Default for LoggingSection {
    fn default() -> Self {
        LoggingSection {
            level: "info".into(),
            requests: false,
        }
    }
}

/// `[forge]`: the Beehive forge (`docs/spec/server.md` §8).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ForgeSection {
    /// Run the forge and serve its services.
    pub enabled: bool,
    /// Forge database: `sqlite:<dir>` or a `postgres://…?schema=<name>` URL; empty =
    /// `<data_dir>/forge` (SQLite mode) or schema `bee_forge` (or `<postgres_schema>_forge`)
    /// of the metadata Postgres database.
    pub database: String,
    /// How long an admitted (principals, orgs, address) decision is reused; 0 = never. Writes
    /// to admission state through this server clear the cache at once.
    pub admission_cache_ms: u64,
    /// How long a repo-access decision input (the forge's repo record, a principal's repo
    /// role) is reused; this bounds how long a stale allow outlives a change made through
    /// another process. Changes made through this server clear it at once.
    pub access_cache_ms: u64,
    /// How long after it was fetched a repo-access input may still be served while the forge
    /// cannot answer (authorization fails closed after that, and for callers with none).
    pub access_outage_grace_ms: u64,
    /// Let callers without credentials read public, non-deleted repos and the public
    /// discovery surfaces (trending, topics, leaderboards, profiles). Off: every call needs a
    /// token, as before.
    pub allow_anonymous_public_read: bool,
    /// Bill plans, invoices and payments (self-hosted servers meter usage and enforce budgets
    /// without billing).
    pub billing_enabled: bool,
    /// Principal ids of site administrators (moderation site queue, takedowns).
    pub site_admins: Vec<String>,
    /// Interval of the VCS event bridge.
    pub bridge_interval_ms: u64,
    /// Outbox relay poll interval.
    pub relay_poll_ms: u64,
    /// Webhook and notification worker poll interval.
    pub webhook_poll_ms: u64,
    /// Agent runner: `command` (run `agent_runner_command` per run), `fake` (the forge's
    /// deterministic runner, for development and demos), or empty (runs fail with
    /// `no_runner_configured`).
    pub agent_runner: String,
    /// Shell command of the `command` runner.
    pub agent_runner_command: String,
    /// Debounce window of agent watchers without their own check interval and of agent feed
    /// triggers, in milliseconds.
    pub agent_debounce_ms: u64,
    /// Domain Pages sites are served under (`<org>.<pages_domain>/<repo>/`).
    pub pages_domain: String,
    /// Address of the host-based Pages listener; empty = no Pages listener.
    pub pages_listen: String,
    /// In-memory DNS and ACME for Pages domains and certificates (development; production
    /// needs real resolver and issuer ports).
    pub pages_memory_dns: bool,
    /// How often the repo mirror worker looks for mirrors due a sync, in milliseconds.
    pub mirror_poll_ms: u64,
    /// Allow `http://` and `file://` mirror remotes (development and tests only).
    pub mirror_allow_insecure: bool,
    /// Serve queued workflow jobs in process on this server. This is intended only for
    /// development because workflow steps execute as host processes.
    pub local_runner: LocalRunnerSection,
}

/// `[forge.local_runner]`: the in-process workflow runner for development.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct LocalRunnerSection {
    /// Whether this server starts local workflow runners. `None` permits `--dev` to supply
    /// its development default while preserving an explicit `false` in a config file.
    pub enabled: Option<bool>,
}

impl LocalRunnerSection {
    /// Resolves the development default while preserving an explicit configuration value.
    pub fn resolve_enabled(&self, dev: bool) -> bool {
        self.enabled.unwrap_or(dev)
    }
}

impl Default for ForgeSection {
    fn default() -> Self {
        ForgeSection {
            enabled: true,
            database: String::new(),
            admission_cache_ms: 1000,
            access_cache_ms: 1000,
            access_outage_grace_ms: 300_000,
            allow_anonymous_public_read: true,
            billing_enabled: false,
            site_admins: Vec::new(),
            bridge_interval_ms: 200,
            relay_poll_ms: 1000,
            webhook_poll_ms: 500,
            agent_runner: String::new(),
            agent_runner_command: String::new(),
            agent_debounce_ms: 60_000,
            pages_domain: "pages.localhost".into(),
            pages_listen: String::new(),
            pages_memory_dns: false,
            mirror_poll_ms: 60_000,
            mirror_allow_insecure: false,
            local_runner: LocalRunnerSection::default(),
        }
    }
}

impl Config {
    /// Resolves the development-only local runner default into an explicit value.
    pub fn resolve_local_runner(&mut self, dev: bool) {
        self.forge.local_runner.enabled = Some(self.forge.local_runner.resolve_enabled(dev));
    }

    /// Parses TOML text, resolving `$VAR` values from `env`.
    pub fn parse_with_env(
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Config, ConfigError> {
        let mut value: toml::Table =
            toml::from_str(text).map_err(|e| ConfigError(e.to_string()))?;
        for (k, v) in value.iter_mut() {
            resolve(v, k, env)?;
        }
        let cfg: Config = toml::Value::Table(value)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parses TOML text against the process environment.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse_with_env(text, &|k| std::env::var(k).ok())
    }

    /// Reads and parses a file.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("cannot read {}: {e}", path.display())))?;
        Config::parse(&text)
    }

    /// The development configuration: SQLite and disk under `data_dir`, generous limits.
    pub fn dev(data_dir: PathBuf, listen: Option<String>) -> Config {
        let mut c = Config::default();
        c.server.data_dir = data_dir;
        if let Some(l) = listen {
            c.server.listen = l;
        }
        c.server.cell = "dev".into();
        c.import.allowed_roots = vec!["/".into()];
        c.forge.pages_memory_dns = true;
        c
    }

    /// Checks cross-field rules serde cannot express.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: &str| Err(ConfigError(m.to_string()));
        let oidc_key = self.auth.oidc_key_pkcs8_hex.trim();
        if !oidc_key.is_empty() && bee_core::hex::decode(oidc_key).is_none_or(|k| k.is_empty()) {
            return bad("auth.oidc_key_pkcs8_hex must be the hex of a PKCS#8 document");
        }
        let t = &self.tracing;
        if !(0.0..=1.0).contains(&t.sample_ratio) {
            return bad("tracing.sample_ratio must be between 0 and 1");
        }
        if !t.otlp_endpoint.is_empty()
            && !(t.otlp_endpoint.starts_with("http://") || t.otlp_endpoint.starts_with("https://"))
        {
            return bad("tracing.otlp_endpoint must be an http(s) URL");
        }
        if t.export_interval_ms < 10 {
            return bad("tracing.export_interval_ms must be at least 10");
        }
        if self.tls.cert_file.is_empty() != self.tls.key_file.is_empty() {
            return bad("tls.cert_file and tls.key_file are set together");
        }
        if (!self.tls.client_ca_file.is_empty() || self.tls.require_client_cert)
            && self.tls.cert_file.is_empty()
        {
            return bad("client certificates need tls.cert_file and tls.key_file");
        }
        if self.tls.require_client_cert && self.tls.client_ca_file.is_empty() {
            return bad("tls.require_client_cert needs tls.client_ca_file");
        }
        if !self.login.oidc_issuer.is_empty() && self.login.oidc_client_id.is_empty() {
            return bad("login.oidc_issuer needs login.oidc_client_id");
        }
        match self.forge.agent_runner.as_str() {
            "" | "fake" => {}
            "command" if !self.forge.agent_runner_command.is_empty() => {}
            "command" => return bad("forge.agent_runner_command is required for runner command"),
            _ => return bad("forge.agent_runner must be empty, `fake` or `command`"),
        }
        match self.metadata.backend.as_str() {
            "sqlite" => {}
            "postgres" if !self.metadata.postgres_url.is_empty() => {}
            "postgres" => return bad("metadata.postgres_url is required for backend postgres"),
            _ => return bad("metadata.backend must be `sqlite` or `postgres`"),
        }
        match self.blobs.backend.as_str() {
            "disk" => {}
            "s3" if !self.blobs.s3_bucket.is_empty() => {}
            "s3" => return bad("blobs.s3_bucket is required for backend s3"),
            _ => return bad("blobs.backend must be `disk` or `s3`"),
        }
        if !matches!(
            self.push_protection.bypass.as_str(),
            "writers" | "admins" | "none"
        ) {
            return bad("push_protection.bypass must be `writers`, `admins` or `none`");
        }
        if !matches!(
            self.logging.level.as_str(),
            "error" | "warn" | "info" | "debug"
        ) {
            return bad("logging.level must be error, warn, info or debug");
        }
        if self.land.batch_limit == 0 || self.land.attempt_budget == 0 {
            return bad("land.batch_limit and land.attempt_budget must be at least 1");
        }
        if self.land.owner_lease_ms < 300 {
            return bad("land.owner_lease_ms must be at least 300");
        }
        if self.admission.reserved_interactive_land >= self.admission.max_concurrent {
            return bad("admission.reserved_interactive_land must be below max_concurrent");
        }
        if self.blobs.signed_url_ttl_secs == 0 {
            return bad("blobs.signed_url_ttl_secs must be at least 1");
        }
        if self.limits.default_deadline_ms == 0
            || self.limits.default_deadline_ms > self.limits.max_deadline_ms
        {
            return bad("limits.default_deadline_ms must be between 1 and max_deadline_ms");
        }
        if !(100..=60_000).contains(&self.limits.forge_lookup_deadline_ms) {
            return bad("limits.forge_lookup_deadline_ms must be between 100 and 60000");
        }
        for s in &self.auth.services {
            if s.name.is_empty() || s.secret_sha256.len() != 64 {
                return bad("auth.services entries need a name and a 64-digit secret_sha256");
            }
        }
        Ok(())
    }

    /// A path inside the data directory unless `explicit` is set.
    pub fn data_path(&self, explicit: &str, default_name: &str) -> PathBuf {
        if explicit.is_empty() {
            self.server.data_dir.join(default_name)
        } else {
            PathBuf::from(explicit)
        }
    }
}

fn resolve(
    v: &mut toml::Value,
    key: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), ConfigError> {
    match v {
        toml::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("$$") {
                *s = format!("${rest}");
            } else if let Some(name) = var_name(s) {
                *s = env(&name).ok_or_else(|| {
                    ConfigError(format!("{key}: environment variable {name} is not set"))
                })?;
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                resolve(item, key, env)?;
            }
        }
        toml::Value::Table(t) => {
            for (k, item) in t.iter_mut() {
                resolve(item, &format!("{key}.{k}"), env)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn var_name(s: &str) -> Option<String> {
    let body = s.strip_prefix('$')?;
    let body = body
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .unwrap_or(body);
    let mut chars = body.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() || first == '_')
        .then_some(())
        .filter(|_| chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .map(|_| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(k: &str) -> Option<String> {
        (k == "PGURL").then(|| "postgres://x@y/z".to_string())
    }

    #[test]
    fn empty_file_gives_defaults() {
        let c = Config::parse_with_env("", &env).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.server.listen, "127.0.0.1:7420");
        assert_eq!(c.land.batch_limit, 512);
        assert_eq!(c.blobs.signed_url_ttl_secs, 60);
        assert_eq!(c.limits.forge_lookup_deadline_ms, 3_000);
    }

    #[test]
    fn forge_lookup_deadline_can_be_overridden_and_has_a_bounded_range() {
        let configured =
            Config::parse_with_env("[limits]\nforge_lookup_deadline_ms = 12000", &env).unwrap();
        assert_eq!(configured.limits.forge_lookup_deadline_ms, 12_000);
        for value in [99, 60_001] {
            let err = Config::parse_with_env(
                &format!("[limits]\nforge_lookup_deadline_ms = {value}"),
                &env,
            )
            .unwrap_err();
            assert!(err.0.contains("limits.forge_lookup_deadline_ms"), "{err}");
            assert!(err.0.contains("100 and 60000"), "{err}");
        }
    }

    #[test]
    fn local_runner_defaults_off_and_rejects_unknown_keys() {
        assert_eq!(Config::default().forge.local_runner.enabled, None);
        let configured =
            Config::parse_with_env("[forge.local_runner]\nenabled = false", &env).expect("config");
        assert_eq!(configured.forge.local_runner.enabled, Some(false));
        assert!(Config::parse_with_env("[forge.local_runner]\nunknown = true", &env).is_err());
    }

    #[test]
    fn local_runner_resolves_development_default_and_explicit_values() {
        let unset = Config::default().forge.local_runner;
        assert!(!unset.resolve_enabled(false));
        assert!(unset.resolve_enabled(true));

        let omitted = Config::parse_with_env("[server]\nlisten = \"127.0.0.1:0\"", &env)
            .expect("config")
            .forge
            .local_runner;
        assert!(omitted.resolve_enabled(true));

        let disabled = Config::parse_with_env("[forge.local_runner]\nenabled = false", &env)
            .expect("config")
            .forge
            .local_runner;
        assert!(!disabled.resolve_enabled(true));

        let enabled = Config::parse_with_env("[forge.local_runner]\nenabled = true", &env)
            .expect("config")
            .forge
            .local_runner;
        assert!(enabled.resolve_enabled(false));

        let mut config = Config::default();
        config.resolve_local_runner(true);
        assert_eq!(config.forge.local_runner.enabled, Some(true));
        config.resolve_local_runner(false);
        assert_eq!(config.forge.local_runner.enabled, Some(true));
    }

    #[test]
    fn edition_defaults_to_enterprise_and_accepts_both_names() {
        assert_eq!(Config::default().server.edition, Edition::Enterprise);
        let parse = |v: &str| Config::parse_with_env(&format!("[server]\nedition = \"{v}\""), &env);
        assert_eq!(parse("cloud").unwrap().server.edition, Edition::Cloud);
        assert_eq!(
            parse("enterprise").unwrap().server.edition,
            Edition::Enterprise
        );
        assert_eq!(Edition::Cloud.as_str(), "cloud");
        assert_eq!(Edition::Enterprise.as_str(), "enterprise");
    }

    #[test]
    fn an_unknown_edition_is_a_config_error() {
        for bad in ["community", "Cloud", ""] {
            let e = Config::parse_with_env(&format!("[server]\nedition = \"{bad}\""), &env)
                .unwrap_err();
            assert!(e.0.contains("edition") || e.0.contains("variant"), "{e}");
        }
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        assert!(Config::parse_with_env("nope = 1", &env).is_err());
        let e = Config::parse_with_env("[server]\nlisten2 = \"x\"", &env).unwrap_err();
        assert!(e.0.contains("listen2"), "{e}");
        assert!(
            Config::parse_with_env("[[auth.services]]\nname='a'\nsecret_sha256='x'\nzz=1", &env)
                .is_err()
        );
    }

    #[test]
    fn oidc_key_must_be_hex() {
        let parse = |v: &str| {
            Config::parse_with_env(&format!("[auth]\noidc_key_pkcs8_hex = \"{v}\""), &env)
                .and_then(|c| c.validate().map(|_| c))
        };
        assert!(parse("").is_ok());
        assert!(parse("3081").is_ok());
        let e = parse("not hex").unwrap_err();
        assert!(e.0.contains("auth.oidc_key_pkcs8_hex"), "{e:?}");
    }

    #[test]
    fn env_vars_resolve() {
        let c = Config::parse_with_env(
            "[metadata]\nbackend = \"postgres\"\npostgres_url = \"${PGURL}\"\n[server]\ncell = \"$$literal\"",
            &env,
        )
        .unwrap();
        assert_eq!(c.metadata.postgres_url, "postgres://x@y/z");
        assert_eq!(c.server.cell, "$literal");
        let e = Config::parse_with_env("[server]\ncell = \"$MISSING\"", &env).unwrap_err();
        assert!(e.0.contains("MISSING"));
    }

    #[test]
    fn cross_field_rules() {
        assert!(Config::parse_with_env("[metadata]\nbackend = \"postgres\"", &env).is_err());
        assert!(Config::parse_with_env("[blobs]\nbackend = \"tape\"", &env).is_err());
        assert!(Config::parse_with_env("[push_protection]\nbypass = \"all\"", &env).is_err());
        assert!(Config::parse_with_env("[land]\nbatch_limit = 0", &env).is_err());
    }
}
