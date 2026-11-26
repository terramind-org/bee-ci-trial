# bee-server

Status: implemented in `crates/bee-server` (layer 7, PLAN task W11). Scope: the single server
binary: the VCS services of SPEC §7–§10 and §13, the required land coalescer (§7.4), identity
and policy enforcement on every RPC (§9), admission control and deadlines (§15.3–15.4), events
(§11.7), the audit log (PRODUCT §3.3), and the transport surfaces of §12. Forge services plug in
through a registry (§8 below).

## 1. Running it

```sh
bee-server --dev                      # SQLite + disk under ./.bee-dev, prints endpoint + token
bee-server --dev --temp               # same, in a fresh temporary directory
bee-server --config bee.toml        # production: Postgres + S3 (or SQLite + disk)
bee-server --config bee.toml --check-config
bee-server --config bee.toml --check-production
```

Flags: `--config <file>`, `--dev`, `--temp`, `--listen <addr>`, `--data-dir <dir>`,
`--check-config`, `--check-production` (print the operational checklist, exit 1 on a failed
error-level item). Exit codes: 0 ok, 1 startup failure, 2 usage or configuration error.

### Dev mode

`--dev` needs no external service. It uses SQLite for metadata (`<data>/meta.db`), a second
SQLite file for server-owned tables (`<data>/server.db`), disk blobs (`<data>/blobs`), and
creates on first start (idempotently on later starts):

- org `dev`, and user `dev` (a human principal, ULID id, administrator of org `dev`, and listed
  in the effective `ops.operators` so the server-wide operator RPCs answer it; being an org
  administrator alone would not, `docs/spec/operations.md` §1.1);
- repo `dev/playground` with its empty root changeset (the fixed id of SPEC §4.4) and `main@1`;
- the forge database under `<data>/forge` with forge rows for `dev` (owned by the dev user) and
  `dev/playground` under the same ids (§8);
- a root Biscuit token for `dev`, valid 30 days, printed on start and written with the endpoint
  to `<data>/credentials` (`endpoint = "…"`, `token = "…"`);
- a second human, user `reviewer` (a member of `dev` with `write` on `dev/playground`, not an
  admin), with its own 30-day token, printed on start and written to the same file
  (`reviewer = "…"`, `reviewer_token = "…"`), so a change the dev user authored can be approved
  by someone else and landed under a policy that requires an approval.

```sh
TOKEN=$(sed -n 's/^token = "\(.*\)"/\1/p' .bee-dev/credentials)
curl -s -X POST http://127.0.0.1:7420/bee.v1.AuthService/WhoAmI \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d '{}'
```

Local-path imports are allowed from any directory in dev mode.

## 2. Configuration

One TOML file. Unknown keys are errors at every level. A string value that is exactly `$NAME`
or `${NAME}` is replaced by the environment variable `NAME` (an unset variable is an error);
a value starting with `$$` stands for a literal `$`. An empty file is a valid configuration.

| Key | Default | Meaning |
|---|---|---|
| `server.listen` | `127.0.0.1:7420` | One listener for every surface |
| `server.public_url` | `http://<bound addr>` | Base URL for signed URLs and `connect_base_url` |
| `server.advertise_url` | `public_url` | Where other processes forward lands when this one owns a coalescer |
| `server.data_dir` | `./bee-data` | SQLite files, disk blobs, keys |
| `server.cell`, `server.region` | `local` | Reported by `GetServerInfo` |
| `server.edition` | `enterprise` | `cloud` (hosted) or `enterprise` (self-hosted); reported as `ServerInfo.edition`. Any other value is a configuration error. `--check-production` adds one item: cloud needs a login method (`login.github_client_id` or `login.oidc_issuer`), enterprise needs `login.oidc_issuer` or an operator (`ops.operators` / `forge.site_admins`) |
| `metadata.backend` | `sqlite` | `sqlite` or `postgres` |
| `metadata.sqlite_path` | `<data>/meta.db` | |
| `metadata.server_sqlite_path` | `<data>/server.db` | Server-owned tables (SQLite backend) |
| `metadata.postgres_url` | — | Required for `postgres`; server-owned tables live in the same database |
| `metadata.postgres_replica_url` | — | Read replica for immutable rows |
| `metadata.postgres_schema` | — | Schema (created if missing) |
| `metadata.max_connections` | 16 | |
| `metadata.query_timeout_ms` | 5000 | Statement deadline |
| `metadata.read_wait_ms` | 50 | SPEC §6.3 replica wait for `min_version` |
| `metadata.stale_read_max_ms` | 300000 | SPEC §15.1: with the primary unavailable, `ResolveRef` returns the last version this process read if at most this old, with `stale = true`; 0 = fail with `UNAVAILABLE` |
| `blobs.backend` | `disk` | `disk` or `s3` |
| `blobs.disk_root` | `<data>/blobs` | |
| `blobs.zstd_level` | 3 | |
| `blobs.s3_bucket`, `s3_region`, `s3_endpoint`, `s3_access_key_id`, `s3_secret_access_key`, `s3_prefix` | —, `us-east-1` | S3-compatible store; `s3_endpoint` switches to path style |
| `blobs.signed_urls` | true | Offer signed URLs for objects ≥ 64 KiB when clients allow them |
| `blobs.signed_url_ttl_secs` | 60 | SPEC §10.5 `signed_url_ttl` |
| `blobs.url_key_hex` | generated `<data>/url.key` | HMAC key of local signed URLs (≥ 32 bytes) |
| `auth.signing_key_hex` | generated `<data>/signing-keys.json` | Ed25519 secret; with it, rotation is in memory only |
| `auth.oidc_key_pkcs8_hex` | generated `<data>/oidc-signing-key.p8` | ES256 (P-256) key of the OpenID provider's JWKS, id_tokens, Actions OIDC tokens and attestations, as hex of a PKCS#8 document; replicas sharing one store must set the same one |
| `auth.signing_key_id` | 1 | Key id of the first key |
| `auth.token_lifetime_secs` | 43200 | Lifetime of tokens minted without an expiry (12 h) |
| `auth.revocation_refresh_ms` | 1000 | Revocations made by other processes are seen within this bound |
| `[[auth.services]]` `name`, `display`, `org`, `secret_sha256` | — | Service identities (secret stored only as SHA-256) |
| `login.github_url`, `login.github_api_url` | `https://github.com`, `https://api.github.com` | GitHub web (device flow) and REST (`/user`, `/user/teams`) bases for human login (`crate::login`) |
| `login.github_client_id` | — | OAuth app of the GitHub device flow; empty answers `Login{github_start}` with `UNIMPLEMENTED` (token exchange needs no app) |
| `login.github_scopes` | `read:user read:org` | Device-flow scopes |
| `login.github_team_sync`, `login.github_team_orgs` | true, — | Replace a principal's external groups with its GitHub teams (`<org>/<team>`) at every GitHub login, optionally only for these orgs |
| `login.default_org` | — | Org new GitHub and OIDC principals belong to |
| `login.oidc_issuer`, `login.oidc_client_id`, `login.oidc_jwks_url`, `login.oidc_groups_claim`, `login.oidc_leeway_secs` | —, —, discovery, `groups`, 60 | External OIDC IdP whose id_tokens `Login{oidc_id_token}` accepts |
| `login.id_token_ttl_secs` | 3600 | Lifetime of id_tokens this server issues as an OIDC provider |
| `tls.cert_file`, `tls.key_file` | — | PEM chain and key: the main listener speaks TLS only (`crate::tls`) |
| `tls.client_ca_file`, `tls.require_client_cert` | —, false | Ask for client certificates signed by these CAs (mutual TLS), optionally requiring one |
| `[[tls.client_principals]]` `subject`, `principal` | — | A verified client certificate whose CN or SAN DNS name is `subject` authenticates as `principal` |
| `forge.mirror_poll_ms` | 60000 | How often the repo mirror worker looks for due mirrors |
| `forge.mirror_allow_insecure` | false | Allow `http://` and `file://` mirror remotes (development, tests) |
| `forge.local_runner.enabled` | false (true with `--dev` unless the config file sets it false) | Serve queued workflow jobs in process. Each queued repository receives a repo runner under `<data>/runner`; jobs execute as the server's OS user with that user's file and network access, not in a sandbox. Any repository on this server with a workflow, including an imported or mirrored repository, can run steps on this machine, so use it only with repositories you trust. Set `[forge.local_runner] enabled = false` to turn it off under `--dev`; `--check-production` fails when enabled and production uses registered runners. |
| `policy.org_default_file` | built-in | Org default policy for every org (SPEC §9.4 format) |
| `land.batch_limit` | 512 | Most lands one coalescer publishes in one CAS |
| `land.attempt_budget` | 16 | CAS attempts per land before `LAND_CONTENTION` |
| `land.owner_lease_ms` | 6000 | Coalescer owner lease, renewed every third |
| `land.warm_blame` | true | After a land, derive in the background the blame of each file it put (at most 32 files of up to 4 MiB per changeset), so the first Blame reads a stored value |
| `land.warm_git` | true | After a land, derive in the background the git ids (`git_mapping` rows) of the new tip, so the first git fetch of it does not hash the tree |
| `land.warm_pack` | true | After a land (and after `land.warm_git`), build in the background the pack a stock `git clone` of the repo asks for and keep it in the gateway's pack cache within its size limits, so the first clone of the new tip is a cache hit (`docs/spec/git-gateway.md`) |
| `limits.default_deadline_ms` | 30000 | Deadline of unary and client-streaming requests that set none, and the time a server-streaming call may take to open; an open stream is bounded only by a deadline its client sent (SPEC §15.3) |
| `limits.forge_lookup_deadline_ms` | 3000 | Deadline of each forge-backed lookup, in milliseconds (100 to 60000); a timed-out read is retried once when the request deadline has room |
| `limits.max_deadline_ms` | 300000 | Longest deadline accepted (`grpc-timeout`, `Connect-Timeout-Ms`) |
| `limits.max_batch_ids` | 1000 | Ids per `GetObjects` / `HasObjects` |
| `limits.max_blob_bytes` | 104857600 | |
| `limits.max_message_bytes` | 67108864 | HTTP/JSON message limit; also gRPC decode limit of `WriteService` |
| `admission.max_concurrent` | 8192 | Requests in flight, all tiers |
| `admission.reserved_interactive_land` | 2048 | Slots only `interactive` and `land` may use |
| `admission.max_bulk` | 256 | |
| `admission.max_anonymous` | 64 | |
| `admission.queue_wait_ms` | 5000 | Wait for a slot before shedding |
| `admission.per_principal_rps` | 5000 | Token bucket per principal (burst 2×); 0 = off |
| `admission.per_root_rps` | 20000 | Token bucket per delegation root; 0 = off |
| `admission.root_max_changesets`, `root_max_landed`, `root_max_upload_bytes` | 0 (off) | Quotas per delegation root, shown by `WhoAmI` |
| `push_protection.enabled` | true | Secret scanning of blobs a changeset adds |
| `push_protection.bypass` | `writers` | Who may bypass with a reason: `writers`, `admins` (`manage_policy`), `none` |
| `import.allowed_roots` | `[]` (dev: `["/"]`) | Directories local-path imports may read |
| `redaction.quarantine_days` | 7 | Quarantine before the scheduled purge |
| `redaction.purge_interval_secs` | 60 | Scheduled purge interval |
| `logging.level` | `info` | `error`, `warn`, `info`, `debug` |
| `logging.requests` | false | One structured line per request |
| `tracing.otlp_endpoint` | empty (no export) | OTLP/HTTP collector base URL; sampled spans go to `<url>/v1/traces` (§6) |
| `tracing.export_interval_ms` | 1000 | Span export interval |
| `tracing.sample_ratio` | 1.0 | Share of new traces (no `traceparent`) sampled, decided from the trace id |
| `tracing.service_name` | `bee-server` | `service.name` of exported spans |
| `server.share_history_rows` | true | Share changesets and graph nodes read by one request with later requests on the same repo (immutable under their id); off, each request reads its own |
| `server.trust_forwarded_for` | false | Take the client address from `X-Forwarded-For` (behind a trusted proxy); otherwise the TCP peer address replaces the header |
| `forge.enabled` | true | Run the Beehive forge (§8) |
| `forge.database` | SQLite `<data>/forge`, or schema `bee_forge` (`<postgres_schema>_forge`) of the metadata Postgres database | `sqlite:<dir>` or `postgres://…?schema=<name>` |
| `forge.admission_cache_ms` | 1000 | Reuse of an admitted (principals, orgs, address) decision; cleared at once by writes to admission state through this server |
| `forge.access_cache_ms` | 1000 | Reuse of repo-access inputs (forge repo records, principals' repo roles, repo name resolutions, the org admin list in the built-in org default policy, and signing keys) (§4.1); bounds a stale allow and a rotated or retired signing key made through another process; cleared at once by access changes through this server |
| `forge.access_outage_grace_ms` | 300000 | How long after it was fetched a repo-access input may still be used while the forge cannot answer; past it, and with none, calls fail closed with `UNAVAILABLE` |
| `forge.allow_anonymous_public_read` | true | Callers without a token may read public, non-deleted repos (VCS read RPCs, the reads behind the repo tabs including its posts, git clone and fetch over HTTP) and the public discovery reads (search, trending, topics, leaderboards, user profiles); every other call and every write still answers `UNAUTHENTICATED`. Off: every call needs a token |
| `forge.billing_enabled` | false | Plans, invoices and payments (usage metering and budgets work either way) |
| `forge.site_admins` | `[]` | Principal ids of site administrators (moderation; also operators, `docs/spec/operations.md` §1.1) |
| `ops.operators` | `[]` (dev mode: the dev user) | Principal ids allowed on the server-wide operator RPCs; org administrators are not operators |
| `forge.bridge_interval_ms` | 200 | VCS event bridge interval |
| `forge.relay_poll_ms`, `forge.webhook_poll_ms` | 1000, 500 | Outbox relay and webhook/notification worker polls |
| `forge.agent_runner` | empty | `command`, `fake` or empty (§8.1 item 6) |
| `forge.agent_runner_command` | — | Shell command of the `command` runner |
| `forge.agent_debounce_ms` | 60000 | Debounce window of agent watchers without their own check interval, and of agent feed triggers |
| `forge.pages_domain` | `pages.localhost` | Pages sites are `<org>.<pages_domain>/<repo>/` |
| `forge.pages_listen` | empty (off) | Address of the host-based Pages listener |
| `forge.pages_memory_dns` | false (dev: true) | In-memory DNS and ACME for Pages domains and certificates |
| `import.github_url` | `https://api.github.com` | GitHub API base for GitHub imports (GHES, tests) |
| `import.github_app_id`, `import.github_app_private_key_file` | — | GitHub App for imports given a `github_installation_id` |
| `import.github_webhook_secret` | — | Secret of `POST /webhooks/github` (empty = not served) |
| `import.github_sync_interval_secs` | 300 | Polling of live GitHub imports (0 = webhooks only) |
| `import.github_oauth_client_id`, `import.github_oauth_client_secret` | — | GitHub OAuth app that verifies `ClaimIdentity` codes |

The built-in org default policy: every authenticated principal reads and writes; principals
listed as org admins and principals whose own forge repo role is admin hold `admin`
(`@repo-role/admin`), those whose own role is maintain hold `maintain` (`@repo-role/maintain`,
SPEC §9.4); `main` and `release/*` have land rules (so they are protected and move only through
Land) with no checks or approvals and agents allowed to land every class. Repos tighten this in
`.beehive/policy.toml`, which takes effect once it lands; a `[members]` role set there
replaces the default's entry for that role.

## 3. Endpoints

One TCP listener serves HTTP/1.1 and HTTP/2 (cleartext, prior knowledge; with `[tls]`, TLS 1.2/1.3
only with ALPN `h2` and `http/1.1` and optional mutual TLS whose mapped client certificates
authenticate requests without a bearer token, `crate::tls`). Requests whose
`Content-Type` starts with `application/grpc` go to tonic; everything else to the HTTP router.

| Path | Surface |
|---|---|
| `POST /bee.v1.<Service>/<Method>` (`application/grpc*`) | gRPC |
| `POST /bee.v1.<Service>/<Method>` (`application/json`, `application/proto`, `application/connect+json`, `application/connect+proto`) | Connect HTTP/JSON gateway (`bee_proto::connect`) over the same service objects |
| `POST/DELETE /mcp` | MCP streamable HTTP (`bee_mcp::mcp_router`), calling this server's HTTP/JSON surface with each caller's own `Authorization` |
| `GET /healthz` | `ok` while the process serves |
| `GET /readyz` | 200 when the metadata store and server tables answer within 2 s and, with `forge.enabled`, the forge has opened; else 503. A forge that failed to open at start is not retried: the process serves the VCS without it and stays not ready until it is restarted. `/healthz` is unaffected |
| `GET /metrics` | Prometheus text (§6) |
| `GET /blob/<org>/<id>?exp=&sig=` | Local signed-URL reads (SPEC §10.5): verified with `bee_store_blob::LocalUrlIssuer`; never serves redacted or quarantined objects |
| `/<owner>/<repo>.git/…` | Git smart HTTP and LFS (`docs/spec/git-gateway.md`) |
| `/scim/v2/{organizations,enterprises}/<name>/…` | SCIM 2.0 (`Scim::handle`, SCIM bearer tokens) |
| `GET /saml/{orgs,enterprises}/<name>`, `GET …/login`, `POST …/acs` | SAML SP metadata, SP-initiated sign-in, assertion consumer |
| `GET /login/oauth/authorize`, `POST /login/oauth/access_token`, `POST /login/device/code` | OAuth code flow with PKCE, refresh and device-code grants for apps and OAuth apps (GitHub response shapes); the `openid` scope adds an ES256 `id_token` (`crate::oidc`, F49-012) |
| `GET /.well-known/openid-configuration`, `GET /.well-known/jwks.json` (= `/login/oauth/jwks`), `GET`/`POST /login/oauth/userinfo` | Beehive as an OpenID Connect provider: discovery, the id_token key, userinfo for a bearer access token |
| `/releases/…`, `/social-previews/…` | Signed release asset URLs; social preview images: `PUT` to the signed URL `CreateSocialPreviewUpload` returns (PNG or JPEG, at most 1 MiB, sets `social_preview_url`), public `GET` |
| `/_evidence/<repo id>/<evidence id>/<asset>/…` | Evidence of change requests (§8.9, `docs/spec/forge-briefs.md` §4): `PUT …/upload` to the URL `BeginEvidenceUpload` returns; `GET`/`HEAD …/as/<principal chain>` at the signed URL every evidence read carries, which decides repo access again for the URL's principal chain; `nosniff`, a sandbox CSP, `attachment` for anything but images, video and audio, an `ETag` and `Range` |
| `/posts/<repo id>/<media id>` | Post media (§8.8): `PUT` to the upload URL `BeginPostMediaUpload` returns; `GET`/`HEAD` at the signed URL every post read carries, which decides repo access again for the URL's principal chain; `nosniff`, a sandbox CSP and `Range` |
| `/v2/…`, `/npm/<owner>/…`, `/maven/<owner>/…`, `/nuget/<owner>/…`, `/rubygems/<owner>/…`, `/cargo/<owner>/…` | Package registries (OCI distribution, npm, Maven, NuGet v3, RubyGems, Cargo sparse) |
| `POST /webhooks/github` | Live sync of GitHub imports (`X-Hub-Signature-256` with `import.github_webhook_secret`) |
| `/_ci/…` | CI's signed transfer URLs (`Ci::serve_transfer`): artifact and cache upload/download, job and run log downloads, the one-time job secrets URL; the URL signature is the authorization (`docs/spec/workflows.md` §8) |

Every response carries `x-request-id` (the caller's, or a fresh one). Successful repo-scoped
RPCs carry the `bee-policy-epoch` and `bee-redaction-epoch` trailers (on HTTP/JSON unary:
`trailer-bee-policy-epoch`, `trailer-bee-redaction-epoch` headers).

### Services

| Service | State |
|---|---|
| `ObjectService` | `GetServerInfo` (anonymous; with `repo`, needs read and reports that repo's epochs), `GetObjects` (streamed, verified bytes, per-item status, access class, signed URLs, resume offset and length), `HasObjects` (false for anything unreadable), `ResolveRef` (`at_version`, `min_version`), `GetTierStats`; `GetDaemonSnapshot` (an `SWSN` bundle of the caller-readable blobs of `main` under the sparse prefixes, by signed URL; `docs/spec/cache-tiers.md` §6) |
| `QueryService` | All RPCs. Unreadable delta entries are dropped with `filtered = true`; a changeset whose every change is restricted hides its meta; restricted tree entries are listed with `restricted = true` and never descended into; reads of restricted paths are `POLICY_DENIED`. `FindCopies` serves exact copies in one repo |
| `WriteService` | All RPCs. Land methods `REBASE` and `SQUASH`; `MERGE_COMMIT` answers `MERGE_IN_STACK` (protected refs take linear stacks) |
| `RefService` | All RPCs; `RenameRef` replays the old ref's log onto the new name as one multi-step update (same targets and landed entries, so a fresh name continues the version numbering and log), then deletes the old ref (rolling the new one back if the old one moved) and leaves a redirect: `ResolveRef` of the old name answers the new ref with `renamed_to` while no ref of the old name exists |
| `AuthService` | `Login` (service secret; GitHub device flow run by the server and GitHub token exchange, both keyed by `github:<numeric id>` with team sync; `oidc_id_token` verifies an external IdP's id_token when `login.oidc_issuer` is set and otherwise accepts a pasted bee token; a GitHub outage fails new GitHub logins with retryable `UNAVAILABLE` while issued tokens keep working — `crate::login`), `WhoAmI`, `CheckAccess`, `CheckObjectAccess`, `CreateAgent`, `MintToken`, `Revoke`, `ListTokens`, `ListSigningKeys`, `GetPrincipal`, `UpdateAgentPrincipal`, `DeactivatePrincipal`, `Logout`, and second factors (TOTP and recovery codes with a login challenge, `docs/spec/operations.md` §6) |
| `AdminService` | `Redact`, `Unredact`, `PurgeNow`, `ListRedactions`, `RunGc`, `RunFsck`, `RotateSigningKey`, `RetireSigningKey`, backups and restores (`docs/spec/operations.md` §4), `GetRepoHealth` (read access: refs, first-parent history depth, files, directories and bytes at a ref, files at or above `large_file_bytes` (default 50 MiB, paths the caller cannot read only counted), and `large_file`, `near_push_limit` (80% of `push.max_file_size`), `large_tree`, `many_files`, `many_refs` warnings) |
| `ImportService` | `StartImport` (git history from a local path or an uploaded pack; GitHub repositories with `source_kind = GITHUB`, §8.6), `GetImport`, `ListImports`, `SetImportPhase`, `CancelImport`, `GetFidelityReport`, `ClaimIdentity`, `ListPlaceholderPrincipals`, exports (`docs/spec/operations.md` §7) |
| forge services | Every service `bee-forge` implements (§8.1), including `CommentService`; `AuditService` adds sinks and exports |
| `SearchService`, `PolicyService`, `OpsService`, `GitService` | All RPCs (`docs/spec/operations.md`) |
| every other service | `UNIMPLEMENTED` until registered (§8.3) |

## 4. Authorization

Every RPC except `GetServerInfo` and `Login` needs `Authorization: Bearer <biscuit>`.

1. **Token.** Verified against the key ring (key id, signature, revocation of any block, expiry,
   block contexts) and cached by token hash; cache hits re-check revocation and expiry.
   Deactivated principals anywhere in the chain are refused (`UNAUTHENTICATED`).
2. **Rate quotas.** One token per request from the principal's and the delegation root's buckets,
   taken from both or neither (`RESOURCE_EXHAUSTED`, `retry_after_ms`, `retry-after`);
   `GetObjects` and `HasObjects` then pay one more per full thousand ids (SPEC §15.4).
3. **Repo gate.** A deleted repo is `NOT_FOUND`; enterprise admission runs for the org that owns
   the repo (so a member without an SSO session is told so); the repo-access decision (§4.1);
   then the token allows the action on the repo (matched by `org/name` or id) and ref, and every
   principal of the chain holds a role granting the action under the effective policy.
4. **Paths.** Writes check every changed path with both token halves (Datalog and scope) and
   the policy (`check_access_chain`); reads filter per path. Object reads follow `object_paths`
   (SPEC §9.3): readable at some readable path, or uploaded by the caller and not yet referenced.
5. **Budgets.** CreateChangeset, Merge and Revert charge one changeset; uploads charge bytes;
   lands charge landed changes. Charges are atomic over every budgeted block of the token and
   the delegation-root quota (`BUDGET_EXHAUSTED` for token budgets, `QUOTA_EXCEEDED` for quotas).
   An identical CreateChangeset retry is not charged. Landed budgets are refused before queueing
   once spent, and charged after publication.

### 4.1 Repo access (SPEC §9.6)

`AppState::authorize_repo(caller, repo, action)` (`crates/bee-server/src/access.rs`) is the one
authoritative decision; `gate` and `gate_token_only` call it first, so every VCS RPC, git smart
HTTP and SSH session (`git_mount`), `GitService` RPC, LFS batch and lock call goes through it. It
reads the forge's record of the repo (`ForgeInputs::access_record`: visibility, `archived`,
`deleted_at`, owning org; a repo of a deleted org reads as deleted; a wiki repo `<name>.wiki`
takes its parent's record) and every chain principal's role (`ForgeInputs::access_role` =
`Orgs::effective_role`, which counts org roles and base role, collaborators, teams, app
installations, deploy keys, fork access and public/internal read), then decides (`access::decide`,
a pure function):

| Repo | Caller | Answer |
|---|---|---|
| deleted | anyone | `NOT_FOUND` |
| not public, a principal of the chain cannot read | — | `NOT_FOUND` |
| archived, action other than `read` | can read | `ARCHIVED` |
| live | role lacks the action | `PERMISSION_DENIED` (audited as `vcs.<action>`) |
| no forge record (VCS-only) | — | policy alone, as without a forge |
| public, live or archived, `read` | no credentials (empty chain) | allowed |
| anything else | no credentials (empty chain) | `NOT_FOUND` |

**Anonymous read** (`forge.allow_anonymous_public_read`, default on). The request layer gives a
request with no credential (no `Authorization`, or a scheme with no token) to one of
`access::ANONYMOUS_READ_METHODS` the server's own read-only token for the principal `anonymous`,
minted in `access.rs`, never returned, and minted again when the signing keys change. The
request is classified first, so it runs in the `anonymous` admission tier and is rate-limited
per client address. `authorize_repo` decides it with the empty chain; the forge's own reads see
a principal with no roles, and `UserService` reads that default to "me" answer
`UNAUTHENTICATED` unless they name a user. git smart HTTP reads (`git_mount`) use the same
token, and any refusal answers 401 so git asks for credentials. Every RPC not on the list, and
every write, still answers `UNAUTHENTICATED` without a token. With the switch off nothing is
injected and every call needs a token.

An agent acts with its own role or that of the next principal of its chain (its owner, from the
principal table when the token names no delegator), whichever is higher, so the chain's humans
and services decide. Read of a public repo needs
no role lookup.

Entry points that do not pass `gate` call the decision themselves: `Rebase` (needs `commit` at
the repo level, as it stores a changeset), `CheckAccess` (the caller must read the repo),
`CheckObjectAccess` (the subject must), `ListRedactions` (read), imports into an existing repo
(`manage_settings`), `ResolveGitSshKey` (operators, or `manage_settings` on the repo), export
download URLs (re-checked on every `GetExport`/`ListExports`), signed URLs (below), LFS transfer
URLs (`GitBackend::signed_history` decides again for the principal the URL was signed for: read,
or `commit` for uploads), search (`readable_repos`, code and topic search filter through `gate`),
and every server stream (the forge re-decides authorization at least every second while open and
ends the stream with the same typed refusal, such as `TOKEN_REVOKED` or `PERMISSION_DENIED`).

**Signed URLs.** Locally served URLs (`/blob/…`) are issued through `AppState::issue_url`, which
appends a grant `g` (base64url JSON of the repo ids and the caller's principal chain) and `gs`
(keyed BLAKE3 of the URL's signature and `g`, key derived from the URL key); `/blob` refuses a
URL without a valid grant (401) and re-runs the decision for each repo (404 when refused, 503
when the forge cannot answer). URLs of an S3 store are bounded by `signed_url_ttl_secs`.

**Resolution.** With the forge running, `AppState::repo` resolves a full name through the forge
first (renames and transfers with their redirects, forks), so a transferred repo answers under
its new name at once; a VCS name the forge now gives to another full name resolves to nothing;
the context's full name (matched by token repo patterns) is the forge's. `policy_org` (the org
default policy) and admission use the owning forge org, so a transferred repo is governed by its
new org immediately. Its VCS repo stays in the namespace it was created in, so objects keep their
ids and storage (SPEC §9.6).

**Caching.** Records, roles and name resolutions are reused for `forge.access_cache_ms`.
`AppState::forget_access` drops them all and is called before the answer leaves for every
non-read call of `RepoService`, `OrgService`, `TeamService`, `EnterpriseService`, `AppService`
and `SponsorService` (the request layer), every non-GET request of the GitHub gateway, and SCIM
writes; the forge event listener `forge::access::AccessInvalidator` also drops them on relayed
`repo.*`, `deploy_key.*`, `org.*`, `member.*`, `collaborator.*`, `repo_invitation.*`, `team.*`,
`app.*`, `role.*`, `enterprise.*`, `sponsor*` and `scim.*` events. A lookup that started before an
invalidation never stores its answer. When the forge fails or times out (3 s), an input fetched
within `forge.access_outage_grace_ms` is used; otherwise `UNAVAILABLE`.

`MintToken` derives the new token from the caller's own token (offline attenuation done by the
server), so it can never exceed the caller's scope and revoking the caller's token revokes it;
for an agent the caller owns it adds `act_as`. `Revoke` accepts the caller's own token ids, the
ids of its ancestors, and tokens minted for the caller or on its behalf.

Policy is the org default overlaid by `.beehive/policy.toml` at the tip of the target ref (`main`
for repo-wide decisions and for refs that do not exist yet), with `CODEOWNERS` when the policy
names it, cached per tip; an invalid policy at a tip falls back to the last good one. The policy
epoch is `repo_epoch(policy hash, revocation counter)`; the redaction epoch is the store's
counter plus quarantine bumps.

## 5. Writes

CreateChangeset validation order (each failure stops the request): repo and `commit` gate;
`import_mode` only for services; ChangeId, parents (changesets, unique, at most 16, at least one
unless importing), delta shape (`validate_delta`), empty delta without `allow_empty`
(`EMPTY_CHANGE`); write access to every changed path; push rules (`POLICY_DENIED` with
`push:*` ids); every put blob live and readable by the caller, else `MISSING_OBJECT` with the ids
(an unreadable blob is indistinguishable from an absent one); secret scanning; meta (server sets
`creator` and `provenance.principal_chain`, refusing a client chain that disagrees); trees built
from the first parent; `expected_root_tree` (`ROOT_TREE_MISMATCH`); idempotency (an existing id is
returned without charging); budget; then the meta and new trees in one upload and the changeset
row with its edges, `object_paths` and eager rows (`graph`, `path_history`) in one transaction.

Push protection scans added regular files up to 1 MiB with `bee_secrets::Scanner::builtin()`.
A blocking finding fails with `SECRET_DETECTED` and a `push_protection` payload (path, blob, line,
rule; never the secret). A bypass is requested with the request header
`bee-push-protection-bypass: false_positive | used_in_tests | will_fix_later`; when allowed
by `push_protection.bypass` the write proceeds and the audit log records the reason and the
findings' rules, lines, redacted previews and path digests.

Uploads verify every object against its id (`bee_store::Writer::upload`), refuse redacted and
quarantined ids (`REDACTED`), enforce `max_blob_bytes`, and record the uploader.

## 6. Observability

Metrics (fixed names): `bee_up`; `bee_rpc_requests_total{method,code}`;
`bee_rpc_duration_seconds{method}` (histogram); `bee_inflight_requests{tier}`;
`bee_admission_shed_total{tier,why}`; `bee_land_batches_total`; `bee_land_batch_size`
(histogram); `bee_lands_total{result}` (`landed`, `skipped`, `conflict`, `denied`, `contention`, `error`);
`bee_land_cas_retries_total`; `bee_coalescer_owner_changes_total`;
`bee_events_appended_total`; `bee_audit_entries_total`; `bee_secret_findings_total{outcome}`;
`bee_integrity_failures_total{source}` (`blob_store`: a read whose bytes fail verification;
`fsck`: digest, encoding, size or missing-bytes violations). Each integrity failure also writes
an `error` log line from the `integrity` component naming the object, and the `integrity`
component of `GetOpsHealth` is down (reason `integrity_failures`) for an hour after the latest.

Tracing (`crate::otel`): every RPC is one server span. A W3C `traceparent` request header joins
the caller's trace (its trace id, the caller's span as parent, its sampled flag); otherwise the
span starts a trace with a fresh 128-bit id, sampled with `tracing.sample_ratio`. Responses carry
`traceresponse: 00-<trace id>-<span id>-<flags>`. With `tracing.otlp_endpoint`, sampled spans are
exported in batches as OTLP/HTTP JSON (kind server; attributes `rpc.system`, `rpc.service`,
`rpc.method`, `bee.code`, `bee.request_id`; resource `service.name`, `service.instance.id`,
`bee.cell`); a full queue (10,000) drops the oldest, a failed export is dropped and logged.
`GetOpsTrace` finds a span by trace id or request id.

Logs are one JSON object per line on stderr with `ts_ms`, `level`, `component`, `msg` and explicit
fields. Paths are logged only as `path_digest` (first 8 bytes of BLAKE3, hex); tokens, secrets and
file contents are never logged.

Every state change writes an audit entry (who: the full chain; what: the RPC; repo; target;
decision; detail; request id) to the hash-chained audit log (`audit/<seq>` in the server tables,
entry `n` storing the hash of `n - 1`, inserted only if absent so concurrent processes cannot fork
the chain). Appends are batched into one transaction per drain and acknowledged before the RPC
answers. `bee_server::audit::AuditSink` is the seam a forge audit module replaces.

Events: `ref.updated` events (ref, old, new, version, principal chain, landed changes, request
id) are written inside the ref CAS transaction, so a coalesced batch of K lands costs one counter
update; `redaction.*` and `import.finished` events are written under sequence numbers reserved in
blocks of 64 (`events::EventLog`).

## 7. The land coalescer (SPEC §7.4)

- **Routing.** Land on a protected ref (policy land rule or active ruleset, or the `main` /
  `release/*` namespace) goes to the ref's coalescer; any other ref uses the plain algorithm.
- **One fenced owner.** Ownership is a lease row `lease/<repo>/<ref>` in the server tables:
  owner process, fence, expiry, endpoint. Taking a free or expired lease increments the fence.
  The owner renews every third of the lease and re-checks (renewing) its fence immediately before
  every publication; if the row names another owner or another fence it publishes nothing.
- **Batching.** The owner takes every waiting land (up to `land.batch_limit`). For each, in
  arrival order: expected version; the stack (first-parent chain to the tip; merges are
  `MERGE_IN_STACK`); the landed index and lands earlier in the batch (same source: skipped;
  different source: `ALREADY_LANDED`); token scope over the stack; `evaluate_land` with diff
  classes when a rule needs them and forge inputs (reviews, checks, leases); `rebase_stack` onto
  the running tip (the previous stack's result). Conflicts (`MERGE_CONFLICT` with the conflict
  set) and denials (`POLICY_DENIED` / `LEASED` with the missing list) fail only their own stack.
  The batch rebases over one buffered `History` (`History::buffered`, a `bee_store::StagedMeta`):
  every tree, changeset and eager row it creates is read back from memory by the next stack and
  written with one `MetaStore::put_batch` transaction before the CAS (one durable commit for the
  whole batch instead of two per rebased stack).
- **One CAS.** Accepted stacks become K steps of one `MetaStore::update_ref`: versions
  `v+1 … v+K`, K ref-log entries, every landed row and K `ref.updated` events in one transaction.
  Stack i answers with version `v+i`. A version mismatch or landed-index race retries the whole
  batch from a fresh read with full-jitter backoff (2 ms doubling to 200 ms) up to
  `land.attempt_budget`, then `LAND_CONTENTION`.
- **Idempotency.** A retry after "committed, response lost" finds every member in the landed
  index and returns the recorded result (`skipped = true`).
- **Failover.** A process that is not the owner forwards lands over gRPC to the owner's
  endpoint (with the caller's token, `x-bee-forwarded: 1`, and the owner's identity:
  `x-bee-coalescer-owner` = its instance id, `x-bee-coalescer-fence` = the fence read). The
  request layer refuses a forward naming another instance before authenticating it, and the
  owner takes one only while its lease still holds that fence (then queues it to its own
  coalescer); both refusals are `UNAVAILABLE` / `not_coalescer_owner`. If the owner cannot be
  reached, or another process answers at its endpoint, the forwarder lands the waiting batch
  itself with the plain CAS algorithm (safe: the ref CAS and the landed index make every path
  linearizable; the lease exists for throughput). After the dead owner's lease expires the next
  process takes it with a higher fence; a lease whose endpoint answered as another process is
  taken over at once (safe by fencing). Graceful shutdown releases leases.
- **Ref-log principal.** `RefUpdate` has one principal for all K entries. When a batch mixes
  principals the ref log records `coalescer:<instance>`; each stack's principal chain is in its
  `ref.updated` event and in the audit log. This deviates from "each stack's ref-log entry is the
  same as if it had landed alone" (SPEC §7.4) until `RefStep` gains a principal (a `bee-store`
  change outside this task).

## 8. The forge (Beehive)

`bee_server::forge` builds the Beehive forge (`bee-forge`) over the server's stores at
startup (`forge.enabled`, default on) and mounts every forge service on the same
`ServiceRegistry` as the VCS services, so each forge RPC is served on gRPC, HTTP/JSON and `/mcp`
behind the same request layer (request ids, deadlines, admission, metrics, epoch trailers).

### 8.1 Wiring

`ForgeWiring::open` (`crates/bee-server/src/forge/mod.rs`):

1. **Store.** `ForgeStore` on its own database (`forge.database`; default SQLite files under
   `<data>/forge`, or schema `bee_forge` of the metadata Postgres database). Tests may inject
   a store (`ServerOptions::forge_store`).
2. **Hooks** (`forge/adapters.rs`), all over the server's VCS stores: the principal directory
   (server principal table, logins = display names, plus principals learned from tokens),
   `ServerProvisioner` (org rows; repos with their root changeset and `main`, README/license on
   `auto_init`, template copies as one changeset), `ServerResolver` (SPEC §5 revisions),
   `ServerPolicy` (the effective policy the coalescer evaluates), and the community ports:
   `ServerContent` (wiki repo `<org>/<repo>.wiki`, gist repos `-gists/<gist id>`, files of real
   repos), `ServerTags` (`tags/*` refs, annotations in the server tables), `ServerActivity`
   (history from the VCS, CRs and issues from the forge), the changes module as release notes'
   change source, and HMAC-signed blob URLs keyed from the server's URL key.
3. **Modules.** `Forge::open`; `Enterprise` (+ `attach`); `Moderation` installed as the write
   guard, with account age from the principal ULID and contributions from landed CRs; `Billing`
   (+ `UsageIngestor`); `Apps` (installation and OAuth tokens minted with the server's signing
   keys, revoked in the server's revocation list); `Models`; `Changes` with
   `ChangeHooks { vcs: HistoryVcs, lander: ServerLander, teams: enterprise teams }` and
   `changes.attach` (auto-merge); `forge.set_changes` (`RepoService.GetContext` delegates to it);
   `Work::set_hooks` with issue templates and `ServerBranches` (`CreateIssueBranch`); `Ci`
   (`HistoryCiVcs`, logs/artifacts/caches in the blob store, HS256 job-token key and secret
   cipher derived from the server's URL key, job tokens signed with the server's keys, URLs under
   `server.public_url`) with `ci.attach` and `ci.spawn_worker` (1 s tick, cron minutes);
   `Security` (`HistorySecurityVcs`; no advisory feed, registry or verifier configured yet) with
   `security.attach` and `maintain` every minute; `Bisector` (post-land bisect and automatic
   revert, `docs/spec/workflows.md` §13) with `attach` and `tick` every 5 s.
4. **Registration.** `OrgService`, `RepoService`, `LeaseService`, `CheckService` (as
   `BisectCheckService`, which serves the bisect RPCs and delegates the rest),
   `EventService`, `AuditService`, `IssueService`, `ProjectService`, `DiscussionService`,
   `WikiService`, `GistService`, `ReleaseService`, `NotificationService`, `UserService`,
   `InsightsService`, `SponsorService`, `ChangeService`, `TeamService`, `EnterpriseService`,
   `AppService`, `BillingService`, `ModelService`, `ModerationService`, `WorkflowService`,
   `RunnerService`, `SecretService`, `DeploymentService`, `SecurityService`, `AgentService`,
   `SwarmService` (`docs/spec/agents.md` §10), `WorkspaceService`, `PackageService`,
   `PagesService`, `BriefService` (evidence bytes go to the CI blob store and its URLs are
   signed with a key derived from the CI key; the maintenance loop calls `Briefs::maintain`
   every 60 s to delete abandoned uploads), `PostService` (posts are attached to the changes module with
   `Posts::attach`, so lands, closes and reopens update a change request's post in their own
   transaction; media bytes go to the package blob port, URLs are signed with a key derived
   from the forge's URL key).
7. **Packages and Pages** (`forge/packages.rs`): `Hooks.packages` = a key/value `BlobPort`
   over the blob store (content-addressed blobs of the internal `-packages` org, key → blob and
   size in the server tables), the server's HMAC URL signer, team grants from enterprise teams;
   `Hooks.pages` = artifacts from CI (`Ci::get_artifact` / `artifact_bytes`), the deploy gate
   from CI environments (an environment with reviewers or protection rules answers
   `APPROVAL_REQUIRED`), in-memory DNS and ACME when `forge.pages_memory_dns` (dev mode),
   detached otherwise; VCS and blobs default to the community VCS and the package blobs.
   `PackageRegistry::handle` is mounted at `/v2/` and `/npm|maven|nuget|rubygems|cargo/<owner>/`
   on the main listener (credentials as the registry reads them; server tokens also pass
   enterprise admission first); `forge.pages.respond` answers every request of the host-based
   Pages listener (`forge.pages_listen`; `X-Forwarded-Proto: https` marks HTTPS; a bearer
   token opens private sites). The real-client tests of `bee-forge`
   (`tests/packages/client.rs`) run against a dev server with `BEE_TEST_CLIENTS=1
   BEE_TEST_REGISTRY=<server url> BEE_TEST_TOKEN=<token> BEE_TEST_OWNER=dev`.
6. **Agents** (`forge/agents.rs`): `Agents::new(forge, changes, ports, config)`, `attach()`
   after the work hooks (it installs the agent gate), both services, `spawn_workers()`. Ports:
   `ServerRunIssuer` (`KeyRingIssuer` over the server's signing keys; revocations go to the
   persistent revocation list), `ServerAgentBlobs` (logs and transcripts as blobs of the
   internal `-agents` org), `ServerAgentRepo` (reads through the VCS; workspace changesets
   through `WriteService.CreateChangeset` with a token for the run's chain, so push protection,
   provenance, budgets and audit apply), `LocalProvider` workspaces under `<data>/workspaces`.
   Runner: `forge.agent_runner = "command"` runs `forge.agent_runner_command` with `sh -c` per
   run in `<data>/agent-runs/<run>` (prompt on stdin; environment `BEE_ENDPOINT`,
   `BEE_TOKEN` = the run token, `BEE_REPO`, `BEE_RUN_ID`, `BEE_BASE_REF`,
   `BEE_WORK_REF`, `BEE_RUN_CHAIN`, `BEE_STEER_FILE`; exit 0 succeeds, cancel and the
   deadline kill it); `"fake"` uses the forge's deterministic `FakeRunner` (development and
   demos only); unset, runs fail with `no_runner_configured`. Agent definitions are managed with
   `AgentService.RegisterAgent` and friends (`docs/spec/agents.md` §2).
5. **Workers.** The forge's own (outbox relay per surface, webhook dispatch and delivery,
   notification fan-out, maintenance), plus the VCS event bridge (repos touched by lands and ref
   writes, and before every `EventService` read of a repo, so forge and VCS events share one
   per-repo log), the merge-queue worker, SSO session sweeps and GitHub import polling.

Dev mode gives `dev` and `dev/playground` their forge rows under the same ids (the dev user owns
the org). Forge org owners and admins are admins in the built-in org default policy too (the
cached org policy is dropped on `member.*` events).

### 8.2 Hooks on the VCS path

Every hook goes through `state::ForgeInputs` (implemented by `forge::ForgeLink`), is bounded by a
short deadline, and fails open on retryable forge errors:

| Hook | Called from | Does |
|---|---|---|
| `access_record`, `access_role`, `repo_by_full_name` | `AppState::authorize_repo` and `AppState::repo` (§4.1) | The forge's repo record, a principal's effective repo role, and full-name resolution; errors are not failed open (the decision fails closed past the outage grace) |
| `admit` | `AppState::gate` / `gate_token_only` (every repo RPC and git smart HTTP) | Enterprise admission for the org that owns the repo: deprovisioned or blocked principals (`PERMISSION_DENIED` / `principal_blocked`), IP allow lists (`IP_NOT_ALLOWED`), SSO and 2FA enforcement |
| `land_inputs` | coalescer policy evaluation | Reviews of the CR (current-revision reviews count for the landed head, so squash and rebase keep approvals), latest check per name of the CR head and the landed head, live leases |
| `landed` | `WriteService.Land` (API, git pushes, CR lands) | Open CRs on the ref whose ChangeIds all landed are recorded landed (`cr.landed`: issues close, leases release); lands on behalf of a CR are recorded by the forge itself |
| `land_conflict` | failed lands | The open CR whose head conflicted holds the conflict |
| `ref_moved` | `RefService` writes, git pushes | A CR opened from that ref gets a new revision; `push.autoChange` (§8.5) |
| `org_admins` | the built-in org default policy | Forge owners and admins administer the org's repos |
| `spend` | `UploadObjects` | Storage budgets and spending limits (`QUOTA_EXCEEDED`) before storing; usage recorded after |
| `protects` / `protect` | `CreateChangeset`, git push (both through push protection), `Land` | `Security::protect` when the repo enables secret-scanning push protection: custom patterns and delegated bypass requests (header `bee-push-protection-bypass-id`); `SECRET_DETECTED` blocks; a bypass reason this server allows (`bee-push-protection-bypass`, audited) covers it too |

After a land, what landed is secret-scanned (`Security::scan_changeset`, alerts only).

Forge services get the same at the request layer (`forge/admission.rs`): the token is verified on
the server's path (deactivated principals are refused), admission runs for the orgs the message
names (its top-level `repo` and `org`, read by transcoding the request with the descriptor
table), `ChangeService.SubmitReview`, reactions and `RepoService.ForkRepo` pass
`Moderation::check_write` (issue and CR creation and comments are guarded by the forge itself),
and `EventService` reads bridge the repo's VCS events first. Calls to `EnterpriseService`,
`TeamService`, `ModerationService`, `OrgService`, `AppService`, SCIM writes and SAML sign-ins
clear the admission cache. App JWTs are left to the forge (`AppService` verifies them).

The client address is the TCP peer (`X-Forwarded-For` is overwritten with it) unless
`server.trust_forwarded_for` is set.

### 8.3 What still answers `UNIMPLEMENTED`

`crates/bee-server/tests/forge_services.rs` calls every RPC of the descriptor on HTTP/JSON
(`AuthService.Logout` last, since it revokes the calling token) and asserts that exactly this list
answers a bare `UNIMPLEMENTED`. An `UNIMPLEMENTED` that carries a subcode (`served_elsewhere`,
`no_comment_source`) is a deliberate answer of a served RPC on that deployment, not a missing
handler. Every other service of the descriptor is registered, and search, policy, ops, the git
RPCs, comments, audit sinks and exports, backups and restores, second factors and repo exports
are served (`docs/spec/operations.md`). Nothing answers a bare `UNIMPLEMENTED` any more: the
list is empty and the test keeps it that way.

The tasks that serve them remove their entries from the list and from this section. A service
added to the protos later answers `UNIMPLEMENTED` (gRPC router fallback and the gateway's
unknown-service answer) until a task registers it, and joins the list until then.

Forge RPCs that a forge module deliberately serves elsewhere answer `UNIMPLEMENTED` /
`served_elsewhere` only past validation (for example `RepoService.ForkRepo`, model inference);
each module's spec lists them. Registering a later service:

```rust
let mut opts = ServerOptions::new(config);
opts.extra = Some(Box::new(|registry| {
    registry.add(WorkflowServiceServer::new(Workflows::new(registry.state().clone())));
}));
```

Handlers reach the stores, policy and authorization helpers through `AppState` (`caller`, `repo`,
`gate`, `require_paths`, `audit`, `events`, `hub`).

### 8.4 Failure domain

The forge database is a separate failure domain (SPEC §15.1). With it down, forge RPCs answer
`UNAVAILABLE` (retryable) while VCS reads, writes and lands keep working for callers whose repo
access is known (decided within `forge.access_outage_grace_ms`; repo access fails closed, so a
caller with no known decision gets `UNAVAILABLE`): admission, land inputs and post-land hooks
fail open (rules that need approvals or checks then deny, leases do not
block), and VCS events wait in the VCS store and reach the forge log after recovery. If the forge
cannot open at startup the server serves the VCS without it. Proven by
`forge_flows::the_vcs_reads_writes_and_lands_while_the_forge_database_is_down`.

### 8.5 Landing change requests

`ServerLander` lands a CR through `WriteService.Land` with a short-lived token minted for the
deciding principal chain (the land request carries `change_request_id`), so CR lands, auto-merge
and the merge queue go through the required coalescer, squash, budgets and audit like any land;
`MERGE_COMMIT` answers `MERGE_IN_STACK`. The merge-queue worker wakes on `merge_queue.*`,
`check.updated` and `cr.landed` and steps each woken queue with `Changes::step_queue`: by default
it waits while the head entry misses only checks, and otherwise lands it or ejects it with the
reason. With the repo setting `merge_queue_check_combined` the head entry is first rebased onto
the tip and checked as `CHECKING` (the `merge_group` workflows run on its `speculative_head`), and
the land is of that exact changeset; `land_inputs` then reads the combined result's checks before
the CR head's (`docs/spec/forge-changes.md`). The forge writes `green/<default ref>` itself with
plain CAS through the VCS writer as post-land checks pass (`docs/spec/workflows.md`).

`push.autoChange`: a git push of a non-protected branch opens a CR against the default ref when
the repo setting `push_auto_change` is on or the push carries `-o bee.cr` (`-o
bee.cr.title=<title>`, `-o bee.cr.draft`); later pushes to the branch revise it. Push options
reach the server with the first ingested commit, so `-o bee.cr` on a push without new commits
is ignored (the repo setting still applies).

### 8.6 GitHub imports

`StartImport` with `source_kind = GITHUB` (`github_token`, or `github_installation_id` with
`import.github_app_*`) runs `bee_github::Importer` with a `StoreTarget` over the server's
stores (eager derived rows, pull refs included) and a `HubSink` over the forge, with its state in
`<data>/github-imports/<id>`; `GetFidelityReport` serves its report, `ListPlaceholderPrincipals`
its placeholders, and `ClaimIdentity` exchanges a GitHub OAuth code for the account and claims it
in every import here. Live sync: `POST /webhooks/github` dispatches by repository to the import's
`Importer::webhook` (a server route rather than `live::webhook_router`, which serves one
importer), and imports are polled every `import.github_sync_interval_secs`. Imports started
before a restart are not resumed until `StartImport` is called again (it resumes from the state
directory).

### 8.7 Deviations and gaps

- `Apps` mints with a copy of the signing keys loaded at startup; `RotateSigningKey` updates the
  forge's verification keys (`Authenticator::set_keys`), but apps keep signing with the key that
  was current at startup until restart.
- App tokens (for `applies_to_installed_apps`) are recognised by a service principal of the
  chain that is an installation.
- Admission decisions are cached for `forge.admission_cache_ms` per (principals, orgs, address);
  changes made through this server clear the cache, changes made through another process are
  seen within that bound.
- `docs/spec/git-gateway.md` §6 still lists `push.autoChange` as not served; it is served here
  (§8.5).
- CI transfer uploads are buffered in memory up to `MAX_TRANSFER_BYTES` (2 GiB) per request.
- The forge push-protection bypass id travels in a request header; git pushes do not carry it
  (they use the server's own bypass push option).
- `AppState::gate` appends denials of state-changing VCS actions (`PERMISSION_DENIED`,
  `POLICY_DENIED`) to the forge audit log as `vcs.<action>` through `ForgeInputs::denied`
  (bounded by 2 s, never failing the call); `AuditService` queries with `denied_only` return
  them with the forge's own denials.
- `ListTokens` serves the scope a token was minted with (repos, paths, refs, actions, budget);
  root and login tokens report only their expiry.
- The operations surface (`docs/spec/operations.md` §12) keeps logs, traces and metric samples
  in memory per process, backups have no point-in-time recovery between them, second factors are
  TOTP and recovery codes (no WebAuthn), rulesets carry only the rule types land policy can
  enforce (the rest are refused), and `ResolveGitSshKey` resolves deploy keys only.

### 8.8 Post media limits

Posts (`docs/spec/forge-community.md` "Posts and feeds") upload media through
`bee_forge::media`. The limits are data (`media::upload::LIMITS`, `posts::*`):

| What | Limit |
|---|---|
| Image (png, jpeg, gif, webp; sniffed by magic bytes, never SVG, HTML or JavaScript) | 8 MiB each, at most 4 per post, alt text 1–1000 characters |
| Video (mp4, webm) | 64 MiB, exactly one per post and no images beside it |
| Post text | 500 Unicode scalar values, NFC, no control characters but `\n` |
| Post body (Markdown) | 20,000 Unicode scalar values, NFC, `\n` line ends, no control characters but `\n` and tab |
| Post article | title 1–200 characters on one line; Markdown body 100,000 Unicode scalar values |
| Post attachments | 32 per post: any number of change requests, at most one release, one issue and one agent run |
| Pending upload (and its `PUT` URL) | 15 minutes; then `INVALID_STATE` / `upload_expired` |
| Signed download URL | 15 minutes; redeeming it decides repo `Read` for its principal chain again (SPEC §9.6) |
| Unattached media (never attached, or removed from a draft) | deleted after 24 hours by the maintenance worker |
| `PUT` body | read into memory up to the largest limit (64 MiB): the reason video is capped there |

`FinalizePostMedia` compares the client's SHA-256 with the one the server computed while
receiving the bytes (`INTEGRITY` on a mismatch or a size other than the declared one). Served
media carries its sniffed `Content-Type`, `X-Content-Type-Options: nosniff`,
`Content-Security-Policy: sandbox; default-src 'none'` and `Accept-Ranges: bytes`.

### 8.9 Evidence transfers

Briefs and evidence (`docs/spec/forge-briefs.md`, SPEC §11.9) move bytes through `/_evidence/…`,
routed with the other asset URLs (`forge/compat.rs`). The signature is the authorization for an
upload; a download is authorized again on every request:

| Step | Rule |
|---|---|
| Verify | `Briefs::verify_upload` / `verify_download`: unsigned, tampered or the other kind of URL → 403; expired → 403 |
| Re-authorize | The download URL names the principal chain it was issued to (`-`: signed out). The server decides repo `Read` for that chain with `authorize_chain` (SPEC §9.6): a repo made private, archived away or deleted since answers 404 |
| `PUT` | Body up to `Briefs::max_upload_bytes` (100 MiB, the video limit), read into memory; 413 above it, 415 for bytes that are not the declared type, 409 for an item that is not pending or whose upload expired |
| `GET`/`HEAD` | Ready items only (else 404). `Content-Type` is the sniffed type; `X-Content-Type-Options: nosniff`; `Content-Security-Policy: sandbox; default-src 'none'`; `Content-Disposition: attachment` for documents, text and captions; `ETag` = the asset's `sha256:` digest; `Accept-Ranges: bytes`, `Range` → 206 or 416 |

Evidence limits are data in `bee_forge::media` (`docs/spec/forge-briefs.md` §3.2): image
10 MiB, video 100 MiB, audio 25 MiB, document 20 MiB, and 64 items or 512 MiB per change request.

## 9. Errors

Handlers return `errors::ApiError` (a boxed `bee.v1.ErrorDetail`). Domain errors map by reason
name, with `INVALID_ARGUMENT` + `rule = multi_parent | merge_in_stack` from `bee-history`
becoming `MULTI_PARENT` / `MERGE_IN_STACK`, and `LAND_ATTEMPTS_EXHAUSTED` becoming
`LAND_CONTENTION`. Subcodes are snake_case. Typed payloads: `conflicts`, `policy_denial`,
`version_mismatch`, `missing_objects`, `ambiguous_prefix`, `limit_exceeded`, `push_protection`.
`bee_core::Reason` gained the SPEC §12.2 reasons it lacked (`MULTI_PARENT`, `MERGE_IN_STACK`,
`ROOT_TREE_MISMATCH`, `TOKEN_EXPIRED`, `TOKEN_REVOKED`, `QUOTA_EXCEEDED`, `LAND_CONTENTION`,
`CANCELLED`, `NOT_QUARANTINED`, `NOT_REDACTABLE`, `INVALID_STATE`); the proto enum and the SPEC
table already had them.

## 10. Redaction

`Redact` (needs `redact`; `purge_now` also `purge`) takes ids, a path at a revision (optionally
every version of the path) or a changeset's meta. Without `purge_now` the object is quarantined:
bytes move under `q/`, the row turns quarantined, reads answer `REDACTED` everywhere (GetObjects
status, ReadPath, signed URLs, Diff/Blame through quarantined content), re-uploads are refused, and
the redaction epoch increases. `Unredact` restores from quarantine (`NOT_QUARANTINED` otherwise).
`PurgeNow` and the scheduled purge (after `quarantine_days`) run `MetaStore::redact` (permanent
blocklist, purged tombstone, bytes deleted). `effective_by` = now + max(signed URL TTL,
revocation refresh), the propagation bound. Changeset ids never change.

## 11. Imports

`StartImport` with `source_kind = GIT` (or a `source_url` and no GitHub credentials) imports git
history into `<org>/<repo>` given as `source` (`org/repo`, or `repo` with `target_org`), creating
the repo with its empty root when missing. `source_url` is a local path (under
`import.allowed_roots`) or `pack:<blob id>?refs=<blob id>` naming an uploaded packfile and ref list
(`<hex id> <ref>` lines). It needs `manage_settings` in the org. The import runs in the background
with the same eager rows CreateChangeset writes; `GetImport` reports `GIT` while running and `LIVE`
when finished (or `FAILED` with the error); `GetFidelityReport` returns the per-category report.

## 12. Local measurements

Run with `cargo run --release -p bee-server --bin bee-server-bench -- --landers 1000 --samples 200`
(add `--pg` under `crates/bee-store-sql/scripts/pg-test-cluster.sh run …` for Postgres).

Date: 2026-10-18. Machine: Apple M4 Pro (14 cores), 24 GiB, macOS (Darwin 27.0), internal SSD.
One in-process server, gRPC over loopback, client-observed latency; blobs on disk.

SQLite metadata store (dev mode; `synchronous = FULL` with `F_FULLFSYNC`):

| Operation | Target | p50 | p99 | Other |
|---|---|---|---|---|
| CreateChangeset, 1 file | p50 ≤ 10 ms, p99 ≤ 50 ms | 11.81 ms | 18.76 ms | |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms, p99 ≤ 150 ms | 6.02 ms | 9.97 ms | |
| Land, one hot ref, 1000 concurrent landers | ≥ 100 lands/s, p99 time-to-land ≤ 10 s | 6370 ms | 12932 ms | 77 lands/s; 203 CAS batches |
| Log, 100-entry page | p99 ≤ 200 ms | 14.29 ms | 54.95 ms | |
| Blame, 10,000-line file, 50 edits | p99 ≤ 200 ms | 5.95 ms | 40.50 ms | |
| Diff, one 10,000-line file with hunks | p99 ≤ 200 ms | 2.10 ms | 3.07 ms | |

Postgres 17 metadata store (throwaway local cluster with a streaming standby, `fsync = on`,
`synchronous_commit = on`):

| Operation | Target | p50 | p99 | Other |
|---|---|---|---|---|
| CreateChangeset, 1 file | p50 ≤ 10 ms, p99 ≤ 50 ms | 1.52 ms | 5.42 ms | |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms, p99 ≤ 150 ms | 1.49 ms | 3.08 ms | |
| Land, one hot ref, 1000 concurrent landers | ≥ 100 lands/s, p99 time-to-land ≤ 10 s | 1189 ms | 3445 ms | 290 lands/s; 203 CAS batches |
| Log, 100-entry page | p99 ≤ 200 ms | 18.66 ms | 20.49 ms | |
| Blame, 10,000-line file, 50 edits | p99 ≤ 200 ms | 7.83 ms | 9.20 ms | |
| Diff, one 10,000-line file with hunks | p99 ≤ 200 ms | 2.20 ms | 2.52 ms | |

Caveats:

- **Misses on SQLite.** CreateChangeset p50 (11.8 ms vs 10 ms) and hot-ref throughput (77 vs
  100 lands/s, p99 time-to-land 12.9 s vs 10 s) miss on SQLite. A profile shows the time is in
  `F_FULLFSYNC` on commit (more than 90 % of samples in the commit path): CreateChangeset commits
  three times (objects, changeset row, audit), and every rebased stack in a coalesced batch
  commits twice (its new trees, its changeset row) before the batch's single ref CAS. The ref CAS
  itself is amortized (about five lands per CAS here; batches are bounded by how many lands
  arrive while the previous batch runs). Since 2026-10-20 a batch writes all its rebased stacks
  in one transaction (`MetaStore::put_batch`, §7 above); `docs/benchmarks.md` has the
  re-measured numbers.
- **Postgres on macOS** uses `fsync`, which does not flush the drive's write cache the way
  `F_FULLFSYNC` does, so its durability cost is lower than SQLite's here; both runs are single
  machine, single disk. These are not production numbers: the SPEC §16 production gate (three
  zones, synchronous replication) is task G1.
- Latencies include loopback gRPC and the server's own audit and lease writes. Each sample
  set is one run; the history workload is one repo with 50 edits of one file.

The `coalescer` integration test also runs 1,000 concurrent landers through the real server path
in debug builds on SQLite (about 45 lands/s there) and asserts exactly-once landing and a
gap-free ref log, not the throughput target.
