# Operations surface: search, policy, ops, git RPCs, comments, audit sinks, second factors, backups, exports

Status: implemented in `crates/bee-server` (`src/services/{search,policy,ops,vcs_gateway,
audit_sinks,second_factor,export,support}.rs`, `src/backup/`) with additions in
`crates/bee-forge` (`service/comments.rs`, `changes/comments.rs`, `ci/secrets.rs`). Tested end
to end by `crates/bee-server/tests/remaining_services.rs` on `--dev` SQLite (and Postgres with
`BEE_TEST_PG`), over HTTP/JSON and gRPC. The protos are the reference for each RPC; this file
says how this server answers them, what it stores, and where it deliberately stops.

Permission vocabulary used below:

- **reader / maintainer / admin**: the repo gate of `docs/spec/server.md` (token scope plus policy
  roles; `read`, `manage_settings`, `manage_policy`).
- **operator**: every principal of the chain is listed in `ops.operators` or
  `forge.site_admins`, and the token allows `manage_settings`. Administering an org (any org)
  never makes a principal an operator: operator RPCs act on every tenant of the server. Dev mode
  lists the dev user in the effective `ops.operators` (`docs/spec/server.md` §1).
- **org admin of X**: every principal of the chain administers org X itself (the org the
  request names or that owns its target), through the org default policy (forge owners and
  admins) or a server-store org admin row; an admin of another org is refused like anyone else.
- **org manager**: the forge's `OrgPerm::Manage` (org owners and admins); **audit reader**:
  `OrgPerm::ReadAudit`.

Every state change writes an audit entry (the server log for VCS-side changes, the org's forge
log for org-scoped changes such as sinks); every client-visible failure is a SPEC §12.2 reason.

## 1. Registration

`ServiceRegistry::with_core` registers `SearchService`, `PolicyService`, `OpsService` and
`GitService`; `ForgeWiring::register` registers `CommentService` (forge) and wraps the forge's
`AuditService` with sinks and exports. `services::spawn_workers` starts the metrics sampler and,
with the forge, the audit sink delivery worker; the search indexer runs while the service lives.
After this work the only RPCs answering `UNIMPLEMENTED` are the services other tasks own
(`docs/spec/server.md` §8.3).

### 1.1 Who may call the administrative RPCs

Every RPC that used to accept "an administrator of some org" is classified by the data it
touches. **(a) server-wide**: operators only. **(b) org-scoped**: an admin of the org the
request names (`docs/contracts/amendments.md`, 2026-10-21). **(c) enterprise-scoped**: owners
of that enterprise. A denial is `POLICY_DENIED` (rule `operators`, missing `role:operator`; or
rule `token`, missing `token:manage_settings`) or, for org and enterprise checks,
`POLICY_DENIED`/`PERMISSION_DENIED` from the org policy or the forge.

| RPC | Class | Why |
|---|---|---|
| `OpsService.GetOpsClusterStatus` | (a) | the cell: every org, node and config version |
| `OpsService.ListMetricDescriptors`, `QueryMetrics`, `QueryMetricsRange` | (a) | one registry for the process; labels carry every tenant's methods and repos |
| `OpsService.QueryOpsLogs`, `TailOpsLogs` | (a) | the process log ring: every tenant's requests, principals and errors |
| `OpsService.GetOpsTrace`, `SearchOpsTraces` | (a) | spans of every RPC on the process |
| `OpsService.ValidateOpsConfig`, `GetEffectiveOpsConfig` | (a) | the server config (redacted, still server-wide) |
| `OpsService.ListOpsRollouts`, `GetOpsRollout`, `PauseOpsRollout`, `ResumeOpsRollout`, `RollbackOpsRollout` | (a) | binary and config rollouts of the cell |
| `OpsService.CreateOpsIncident`, `UpdateOpsIncident`, `SetOpsStatusComponent` | (a) | the one public status page (listing stays open to any caller) |
| `OpsService.ListOpsBackups`, `ListOpsRestoreDrills` | (a) | backups hold every org's databases and keys |
| `OpsService.GetOpsCacheStats` | (a) | process cache tiers |
| `AdminService.CreateBackup`, `ListBackups`, `StartRestore`, `GetRestore` | (a) | whole-server snapshots and restores (databases, signing keys, blobs of every org) |
| `AdminService.RotateSigningKey`, `RetireSigningKey` | (a) | the key ring signs every tenant's tokens (was: any org admin) |
| `GitService.ResolveGitSshKey` | (a) or repo `manage_settings` | operators (SSH frontends), otherwise the repo gate of the key's repo |
| `AdminService.RunGc`, `RunFsck` | (b) | the named org through its default policy (`purge`, `manage_settings`); operators get nothing extra. Each call appends `admin.gc` or `admin.fsck` to that org's forge audit log, including dry runs, actor chain, scope and count-only outcome. |
| `AdminService.Redact`, `Unredact`, `PurgeNow`, `ListRedactions` | repo | the repo gate of the target repo (`redact`, `purge`, `read`) |
| `ImportService` org imports, `ListPlaceholderPrincipals`, org exports | (b) | `manage_settings` in the named org |
| `AuditService` sinks and exports | (b) or (c) | `OrgPerm::ReadAudit`/`Manage` in the named org, or the enterprise's owners |
| `PolicyService` org rulesets | (b) | `manage_policy` in the named org (reads: `read`) |
| `PolicyService` enterprise rulesets | (c) | writes: every principal of the chain owns the enterprise; reads also admit an admin of one of the enterprise's own orgs (was: of any org) |

## 2. SearchService

### 2.1 Code index

- **Store**: `bee_search::DiskIndexStore` under `search.index_dir` (default `<data>/search`),
  one per server process. Derived data: it can be deleted and is rebuilt from the stores.
- **Indexer**: every `search.poll_ms` it reads each repo's event-log head (lands and ref moves
  append events); for a repo whose head moved it brings the manifests of `search.refs` (default
  `main`) to the refs' current targets with `Indexer::sync_ref` (incremental delta replay,
  `docs/spec/search.md` §2.3). Repos are enumerated from the metadata store's `orgs` table.
- **Queries**: targets are the repos the caller passes the repo gate for; the
  `PathAuthorizer` is the caller's path permission under each repo's effective policy, so a
  restricted file is never read, counted, ranked or hinted at (SPEC §9.3). `SearchCode` pages
  (default 25, at most 100), `StreamSearchCode` streams hits, periodic progress and a final
  summary, `SearchSymbols`, `GetSymbolDefinitions` and `GetSymbolReferences` are search-based
  (`precise = false`); the word at `(line, column)` is read from the file at `at` (permission
  checked) and `cross_repo` widens the targets. `GetDocumentSymbols` extracts the outline from the
  file at the revision itself, so it works for any revision.
- **Failure domain**: when the index cannot be opened or read (`UNAVAILABLE` or `INTEGRITY` from
  the store), code, symbol and navigation RPCs answer `UNAVAILABLE` (retryable), the handle is
  dropped and reopened on next use, and `OpsService.GetOpsHealth` reports the `search` component
  down (not on the critical path, readiness `DEGRADED`). No list, land, read or forge search
  touches the index. `search.enabled = false` turns code search off (`UNAVAILABLE`).

### 2.2 Forge searches

| RPC | Source |
|---|---|
| `SearchIssues` | `IssueService.ListIssues` and `ChangeService.ListChanges` called in-process with the caller's credentials, per readable repo matching `repo:`/`org:`/`user:` (those terms and `is:issue`/`is:pr`/`type:` are stripped; the rest is the lists' own query language) |
| `SearchRepos` | readable repos with their forge records; the compiled repos filter evaluated per repo |
| `SearchChangesets` | the `main` history of readable repos (newest first, at most 2,000 per repo), messages, authors, committers, hashes, dates, merges |
| `SearchUsers` | principals of the server store and orgs (`type:org`) |
| `SearchTopics` | topics of readable repos |
| `SearchDiscussions` | `DiscussionService.ListDiscussions` in-process, then the discussions filter |
| `SearchPackages` | `PackageService.ListPackages` in-process per org, then the packages filter |

At most 1,000 repos are examined per search; `incomplete` is set when a per-repo bound was hit.
Query errors are `INVALID_ARGUMENT` (`bad_query`) with positions; `ParseSearchQuery` reports them
without failing and resolves `@me` to the caller's display name.

### 2.3 Saved searches, trending, topics, collections

- Saved searches are rows of the server store per principal (`saved_search`), names unique per
  principal case-insensitively (`ALREADY_EXISTS`), queries validated and stored normalized
  (`bee_search::validate_saved_search`). Another principal's id is `NOT_FOUND`.
- `ListTrendingRepos` ranks readable repos by first-parent changesets on `main` whose time falls
  in the window, then stars; `built_by` is the top five creators. `ListTrendingDevelopers` ranks
  creators the same way (agents only with `include_agents`). `stars_in_window` and
  `forks_in_window` stay 0: the forge keeps star and fork counts, not their times.
- Topics aggregate repo topics; `search.featured_topics` names the featured (curated) ones and
  `[[search.collections]]` the collections (`org/repo`, `@login` or URL items; repos the caller
  cannot read are left out).

## 3. OpsService

| Area | Behaviour |
|---|---|
| Health | Live probes (2 s each): metadata store, server store, blob store (critical path), forge and search index (not), and `background_jobs` (running backups, restores and exports; down while an audit sink is failing). Readiness `DOWN` when a critical component is down, `DEGRADED` when another is. Any authenticated caller. |
| Cluster | This cell: version, config version (BLAKE3 of the effective config), schema version, status, primary up, orgs, one node. A single-cell deployment is its own directory (`directory_version = single-cell:<cell>`). |
| Metrics | The fixed names of `crate::metrics` (`ListMetricDescriptors`). `QueryMetrics` evaluates the live registry (or the sample at `time`); `QueryMetricsRange` evaluates samples taken every `ops.metrics_sample_ms` (720 kept). Aggregations: sum, avg, max, min, per-second rate over `window_seconds`, and p50/p90/p99 interpolated from histogram buckets. Label matchers support equality, inequality and anchored regexes of literals, `.`, `.*`, `.+` and `|`. More than 11,000 points is `TOO_LARGE`; an unknown name is `INVALID_ARGUMENT` (`unknown_metric`). |
| Logs | Every line the structured logger writes is also kept in a 10,000-entry in-memory ring of the process; `QueryOpsLogs` filters it, `TailOpsLogs` sends a backfill then follows live until cancelled. |
| Traces | One span per RPC (trace id = request id): method, start, duration, status. 10,000 kept. `principal_id` filters match nothing (spans carry no principal). |
| Config | `ValidateOpsConfig` runs the strict config parser and `validate()`, with the key path, line and column of the first error (`unknown_key`, `type_mismatch`, `unset_variable`, `invalid_value`), and returns the normalized TOML with defaults; with `component = "production"` it also runs the operational checklist (`crate::preflight`: Postgres metadata, S3 blobs, TLS, shared signing and URL keys, operators, rate limits, safe import roots, off-machine backups; a read replica, stale reads and a non-debug log level as warnings), each failed item an error or warning with code `production:<id>`, and `valid` false when an error fails (`bee-server --check-production` prints the same list); `GetEffectiveOpsConfig` returns the running config with every secret (`*secret*`, `*password*`, `*token*`, `*key_hex`, private keys, access key ids) and URL credentials replaced by `<redacted>`. |
| Rate limits | The caller's principal and delegation-root buckets (limit = burst, remaining now, reset time); any authenticated caller. |
| Rollouts | Each start records a `binary` and a `config` rollout when the version differs from the last one this cell ran. Pause, resume and rollback follow the state machine; a finished rollout is `INVALID_STATE` (on a single cell, roll back by starting the previous version, which records its own rollout). |
| Status page | Incidents and components are server-store rows (nine default components); creating or updating an incident can set component statuses. Listing is open to any authenticated caller. |
| Backups | `ListOpsBackups` lists `AdminService` backups (`metadata_snapshot`, `verified` once a drill passed); `ListOpsRestoreDrills` lists drill restores with duration and data-loss seconds. |
| Cache tiers | No cache tier runs inside `bee-server`; the list is empty (daemons and regional caches report through their own `GetTierStats`). |

Everything except health, rate limits and the status page lists needs an operator.

## 4. Backups and restores (`AdminService`)

Operators only. `CreateBackup` starts a background job and returns `RUNNING`; `ListBackups`
shows progress (newest first, retention `backup.retention_days`, at most 30).

### 4.1 What a backup holds

- **Databases**, consistent per database: SQLite files (`meta.db`, `server.db`,
  `forge/forge-*.db`) snapshotted with `VACUUM INTO` on a read-only connection (online; a WAL
  database is copied at one transaction); with Postgres, every table of the metadata schema
  (metadata tables and `bee_server_kv`) and of the forge schema exported with `COPY … TO STDOUT`
  inside **one** `REPEATABLE READ` read-only transaction, so all of them reflect one snapshot. No
  `pg_dump` is needed.
- **Data-directory keys**: `signing-keys.json` and `url.key`, so tokens and signed URLs keep
  verifying after a restore. Backups therefore contain key material: protect the target.
- **Blob manifests**: per org, every object id the blob store holds (quarantined objects are
  left out). With the disk blob store the objects are copied to the target (incrementally: an
  object already there is not copied again); with S3 the bucket is the durable copy and the
  manifest lets a restore verify it.

### 4.2 The target

`backup.target`: empty or `dir:<path>` (default `<data>/backups`) or `s3` (the bucket of
`[blobs]` under `backup.s3_prefix`). The target is a blob store: backup files are split into
32 MiB content-addressed chunks under a reserved org (`00000000000000000000000000`), so unchanged
files and chunks are stored once across backups; copied objects keep their real org and id. The
manifest (magic `swarm-backup-manifest-v1`) is itself an object, so `ListBackups` also finds
backups written to the same target by another server (for example the one that died).

### 4.3 Restore

`StartRestore(backup_id, target_cell, drill)` writes a complete data directory at
`<backup.restore_root>/<target_cell>` (default `<data>/restores`; it must be absent or empty,
`ALREADY_EXISTS` otherwise): the SQLite files, the keys and the copied blobs. For a Postgres
backup the rows are loaded into the schemas `restore_<target_cell>` and
`restore_<target_cell>_forge` of the configured database (which must be empty): the stores create
their tables (migrations), every table is truncated, the rows are loaded with
`COPY … FROM STDIN`, and identity sequences move past the loaded keys. Then the restore opens the
restored metadata and blob stores and runs **fsck** over every org (`fsck_clean`, also false when
a manifest object is missing); a **drill** also reads back every ref's tip changeset and root tree
and verifies the restored server store's audit chain (`conformance_passed`), and marks the backup
verified when everything passed. `restore.json` in the directory names the source.

To serve the restored data, start a server with `server.data_dir` set to the restored directory
(and, for Postgres, `metadata.postgres_schema = restore_<target_cell>`).

`point_in_time` must be unset or the backup's own time (`INVALID_ARGUMENT`,
`point_in_time_unavailable`): this server keeps no WAL archive. Postgres point-in-time recovery is
the database operator's (WAL archiving); SQLite has none.

## 5. Audit sinks and exports (`AuditService`)

Query, get, stream and chain verification are the forge's (`docs/spec/forge-core.md`). Sinks and
exports cover an org's log; enterprise logs answer as the forge does (`served_elsewhere`).

- **Sinks** (org managers): rows of the server store; names unique per org. Kinds: `HTTPS`
  (JSON lines, `Authorization: Bearer`), `SPLUNK` (HEC events, `Authorization: Splunk`),
  `DATADOG` (log intake array, `DD-API-KEY`), and `S3`, which on this server writes one JSON-lines
  object per batch, atomically, to a `file:///dir` endpoint under `audit.file_roots` (anything
  else is `INVALID_ARGUMENT`, `bad_endpoint`). `credential_secret` names an org secret of the CI
  vault; its value is read at delivery and never returned. `start_seq` backfills from that
  sequence (0: new entries only).
- **Delivery**: every `audit.sink_poll_ms`, each enabled sink reads up to `audit.sink_batch`
  entries after `delivered_seq`, keeps those matching its action prefixes and delivers them in
  sequence order with their `prev_hash` and `hash`, so the receiver can verify the chain
  (`bee_forge::audit::entry_hash`). A failure (non-2xx, timeout, I/O) is retried after
  `sink_retry_base_ms` doubling up to `sink_retry_max_ms`; `delivered_seq` moves only past
  delivered (or filtered-out) entries, so nothing is skipped. Health: `PAUSED` when disabled,
  `FAILING` after `sink_failing_after` consecutive failures, `LAGGING` when far behind or stalled
  for five minutes, else `HEALTHY`. `TestAuditSink` delivers a synthetic unchained entry.
- **Exports** (audit readers): a background job writes the matching entries oldest first as JSON
  lines (proto3 JSON) or CSV (`seq,id,time,actor,on_behalf_of,action,target,repo,org,request_id,
  result,policy_rule,prev_hash,hash`), stores the file as an object of the org (an upload by the
  requester, so GC collects it like any unreferenced upload) and `GetAuditExport` returns a fresh
  signed URL (`blobs.signed_url_ttl_secs`), the sequence range and `head_hash` (the hash of the
  last exported entry) for offline verification against `VerifyAuditChain`.

## 6. Second factors (`AuthService`)

Human principals acting for themselves. TOTP (RFC 6238, HMAC-SHA1, 6 digits, 30 s, ±1 step,
replay-guarded per factor) and single-use recovery codes (ten per set, stored as BLAKE3 digests, a
new set replaces the old). A token-paste `Login` of a human with an authenticator returns a
`second_factor_challenge_id` (ten minutes, five attempts) instead of the token;
`VerifySecondFactor` answers it with a TOTP or recovery code and returns a token of at most one
hour (never past the presented token's expiry, session `2fa:<factor>`), recorded and audited.
Elevated tokens log in without a new challenge.

Enforcement: the server store is the forge's `TwoFactorDirectory`, so an org or enterprise that
requires 2FA (`EnterpriseService.UpdateTwoFactorRequirement`) refuses members without a TOTP
factor at admission (`TWO_FACTOR_REQUIRED`), and `DeleteSecondFactor` refuses to remove the last
TOTP factor of such a member. Passkeys and security keys are refused (`INVALID_ARGUMENT`,
`unsupported_factor`): no pure-Rust WebAuthn verifier fits the dependency rules, and
`secure_methods_only` requirements are therefore unsatisfiable on this server.

## 7. Exports (`ImportService`)

Org managers who can read every path of each repo. Background jobs, never blocking writes;
`CancelExport` stops one between repos (`INVALID_STATE` once finished).

- `GITHUB_ARCHIVE`: a gzipped tar in the org migration archive layout `bee_github::archive`
  reads. Each repo is a bare repository under `repositories/<org>/<repo>.git/` (`HEAD`, `config`,
  loose refs for branches and tags, one pack with its v2 index) whose objects carry the exact git
  ids the git gateway serves; imported history keeps its original ids and annotated tags. With
  `include_metadata`: `repositories_000001.json` (with labels), `users_000001.json`,
  `milestones_000001.json`, `issues_000001.json`, `pull_requests_000001.json` (head and base
  commit ids), `pull_request_reviews_000001.json`, `issue_comments_000001.json` (issue and CR
  conversation comments) and `releases_000001.json`, linked by URLs under the server's public URL;
  records keyed by ULID get a stable numeric id (BLAKE3). The archive is an org object served by
  signed URL. Round trip: the importer's archive reader parses it with nothing skipped, and
  importing its bare repository (`StartImport`, `GIT`) reproduces every commit id.
- `GIT_PUSH`: the same bare repository pushed with `git push --mirror` (the system `git`, as
  imports use it) to one repo's `remote_url`: `http(s)://` (with `remote_token` as a Basic
  credential header, redacted from errors), `ssh://`, or an absolute local path under
  `import.allowed_roots`.

## 8. PolicyService

- `GetEffectivePolicy`: the normalized effective policy as deterministic JSON, its epoch and
  sources (built-in or configured org default, the repo file with blob and changeset, API
  rulesets) and `last_good_fallback` when the tip's file is invalid. `ValidatePolicy`: the
  candidate (inline or at a revision) over the org default and API rulesets; issues with file,
  key path, line, column and subcode; normalized JSON, resulting epoch, one-line rule summaries
  and, with `replay_sample`, how recent CRs would be decided now and under the proposal.
- `EvaluatePolicyAction`: `land` builds a land context (paths with a diff class, or a real
  changeset's stack and classes; hypothetical approvers and passing checks; the forge's reviews,
  checks and leases) and runs the land evaluator, reporting matched requirements, missing ids,
  evaluate-mode results and whether every blocking ruleset could be bypassed; `push`,
  `force_push`, `create_ref`, `delete_ref` run the ref checks; other actions the access checks.
  Evaluating another principal needs `manage_settings`.
- **Rulesets** (repo `manage_policy`, org or enterprise owners): stored in the server store and
  rendered into every covered repo's effective policy as `[[rulesets]]` named `api-<id>` (repo
  name, org name and repo property conditions are applied per repo; `~DEFAULT_BRANCH` and `~ALL`
  resolve per repo; fnmatch patterns become policy ref patterns by their prefix). Land therefore
  enforces them exactly as SPEC §9.4 rulesets: active blocks, evaluate records `would_block`,
  disabled is skipped, bypass actors (principals; teams as `@org/team` groups) are logged
  `BYPASSED`, and matched refs become protected (deletion, force-push and direct creation are
  refused because protected refs move only through Land). Supported rules: required approvals
  (count, human count, classes, paths, owners or reviewer groups, stale dismissal, last-push
  approval), required checks, signatures, linear history, no deletion/force-push/creation,
  restricted paths and extensions, maximum file size and path length, and the merge queue.
  - **Merge queue** (`merge_queue`, branch rulesets only): renders nothing into the policy; the
    forge enforces it. While an `active` ruleset with the rule covers the repo and its ref
    conditions match a change request's target ref (the same `api-<id>` match land uses), the
    ref is queue-required exactly as with the repo setting `merge_queue_required`: `LandChange`
    is refused with `INVALID_STATE`/`merge_queue_required`, auto-merge enqueues once every
    requirement but checks holds, and a bisect auto-revert is enqueued. `evaluate` and
    `disabled` rulesets never require the queue, and bypass actors do not skip it (the queue is
    a route, not a requirement). The server answers the forge through
    `LandFacts::merge_queue_required`. The queue builds and lands one entry at a time, in queue
    order, each on the tip its predecessor left, with the land method the entry was enqueued
    with, and waits for required checks without a deadline; the parameters map onto that:

    | Field | Accepted | Why |
    |---|---|---|
    | `max_entries_to_build` | refused when non-zero | the queue has no build batches |
    | `max_entries_to_merge` | any | enforced per-ref queue capacity; the smallest non-zero value across matching active rulesets wins, and a full queue returns `RESOURCE_EXHAUSTED` / `merge_queue_full` |
    | `min_entries_to_merge` | 0 or 1 | the queue never waits to form a larger group |
    | `min_entries_wait_seconds` | 0 | the queue does not batch entries before checking |
    | `all_entries_must_pass` | false | entries are tested and landed independently |
    | `merge_method` | unspecified | each entry retains its enqueued method |
    | `check_timeout_seconds` | 0 | required checks are awaited without a deadline |

    Zero is the server default. A value outside the accepted set is refused with
    `INVALID_ARGUMENT`/`unsupported_rule`, naming the field (`merge_queue.<field>: …`); the rule
    on a tag or push ruleset is refused the same way. GetRuleset and ListRulesets return the
    rule as stored.

  Refused with `INVALID_ARGUMENT` rather than stored unenforced: required deployments,
  workflows and code scanning, message patterns, merge methods, conversation resolution
  (`unsupported_rule`), ref exclusions (`unsupported_condition`) and role or app bypass actors
  (`unsupported_bypass_actor`). `UpdateRuleset` compares-and-swaps the version
  (`VERSION_MISMATCH`). Every change drops the cached policies.
- **Insights**: the land coalescer hands each decision log with rulesets to
  `services::policy::record_land`, which records it asynchronously (never delaying the land);
  `ListRulesetEvaluations` filters them. `SimulateRuleset` replays the ref logs' lands in the
  window under the policy in force before each land plus the probe ruleset (in evaluate mode),
  returning the operations it would have blocked.
- Custom roles, built-in roles, the permission catalog and custom org role assignments are the
  enterprise module's (`bee_forge::enterprise::roles`), authorized by the forge.
- `GetCodeowners` (readable paths only), `ValidateCodeowners` (inline or the first CODEOWNERS
  file at a revision) and `GetRequiredSignaturesStatus` (the land rule or active rulesets;
  changesets are `UNSIGNED`, or `UNKNOWN_KEY` for imported commits carrying a signature, since
  this server verifies no signatures).

## 9. GitService

Every RPC opens the session the smart HTTP routes open (`git_mount::open_session`) with the
caller's token: reads need read access to every path, writes go through push protection, budgets,
audit, policy and the Land coalescer. A repo with `git_enabled = false` answers `GIT_DISABLED`.

- `GetGitObjects` streams objects (commits rebuilt exactly, trees, blobs, annotated tags) in
  1 MiB chunks, or sizes only; per-object `NOT_FOUND`, `REDACTED`, `INVALID_ARGUMENT`.
- `GitListRefs` (ls-refs with prefixes, peeled tags, the `HEAD` symref, unborn `HEAD`, virtual,
  pull and replace refs on request, ref versions, capabilities, policy epoch).
- `GitPlanFetch` negotiates (acks, NAK, ready) and plans the pack with wants, want-refs, haves,
  shallow, deepen (depth, relative, since, not) and filters; once ready it stores the pack as an
  org object (an upload by the caller) and returns the cache key and a signed URL.
- `GitResolve` maps up to 1,000 ids either way; `imported` marks commits rebuilt byte-for-byte
  from an imported original.
- `CreateGitPackUpload` reserves a signed PUT URL on the LFS transfer route keyed by the pack's
  SHA-256 (`upload_id`); `GitIngestCommits` runs receive-pack over that pack or inline bytes
  (at most 1 MiB), reporting per-ref results (`LAND`, `WORKSPACE_UPDATE`, `REF_UPDATE`,
  `REF_DELETE`) and the ChangeIds of pushed commits; with `atomic`, any failure rejects all.
- Settings: `git_enabled` and `push_auto_change` are the forge's repo settings; `lfs_enabled`,
  `ssh_enabled`, `require_change_id_trailer`, `min_client_version` and `seed_packs_enabled` are
  server rows. The object format can change only while the repo has no history, and new history
  is SHA-1. Every setting is enforced on every transport (smart HTTP, LFS, SSH and these RPCs),
  after the repo gate: git off is `GIT_DISABLED`; SSH off refuses SSH sessions (`GIT_DISABLED`,
  `ssh_disabled`); LFS off refuses the LFS batch and locks APIs, `git-lfs-authenticate` and
  `GitLfsBatch` (`INVALID_STATE`, `lfs_disabled`); a `git/<version>` HTTP user agent older than
  `min_client_version` (2.30.0 when unset) is `INVALID_STATE` (`git_too_old`); with
  `require_change_id_trailer` a pushed commit whose last paragraph has no `Change-Id:` trailer is
  `INVALID_ARGUMENT` (`change_id_trailer_required`). `seed_packs_enabled = true` is refused
  (`INVALID_ARGUMENT`, `unsupported_setting`) because no packfile-uris or bundle-uri is served.
- `GitLfsBatch`, `GitPreflight` (git version, transport, protocol, credential helper, LFS, read
  and push access), `CreateGitCredential` (an attenuated token: read, plus commit/land/lease with
  `write`; at most the caller's expiry) and `ResolveGitSshKey` (repo deploy keys by SHA-256
  fingerprint; called with a service or operator token; the session token acts as the key's
  service principal `deploy-key:<id>`, limited to the repo and read-only when the key is). User
  SSH keys are the identity service's; until it serves them they resolve as unknown
  (`UNAUTHENTICATED`).

## 10. CommentService

Served by the forge (`bee_forge::service::comments`), routing a comment id to its store:
discussion comment ids (`<home>.<number>.<ulid>`), issue comments (the work lookup), and change
request conversation comments, which gained a by-id lookup table (changes migration 2, written
since then; earlier comments are reachable through their CR) plus edit history, deletion and
reaction listing. Authorization is the owning service's: reads need repo read (or discussion
access); edits and deletes are the author's or a triager's. Reactions and read markers take
`ISSUE`, `CHANGE_REQUEST` (by id, or number with `repo`), `DISCUSSION` and `COMMENT` subjects.
`MarkSubjectRead` marks the caller's notification thread of the subject read (a subject without a
thread is already read). CR review-thread comments are edited through `ChangeService`.

## 11. Configuration added

```toml
[search]
enabled = true
index_dir = ""          # default <data>/search
poll_ms = 500
refs = ["main"]
featured_topics = []
# [[search.collections]] slug, display_name, description, image_url, items

[backup]
target = ""             # "", "dir:<path>" or "s3"
s3_prefix = "bee-backups/"
restore_root = ""       # default <data>/restores
retention_days = 30

[audit]
file_roots = []
sink_poll_ms = 1000
sink_batch = 200
sink_timeout_ms = 10000
sink_retry_base_ms = 1000
sink_retry_max_ms = 300000
sink_failing_after = 3

[ops]
operators = []
metrics_sample_ms = 0   # 0 = 10 s
```

The whole config now serializes (`GetEffectiveOpsConfig`).

## 12. Limits and deviations

- Code navigation is search-based (no precise index); trending has no per-window star counts.
- Traces are one span per RPC; logs and traces are per process and in memory.
- Backups are consistent per SQLite database (all-at-once on Postgres); no point-in-time
  recovery between backups.
- Second factors are TOTP and recovery codes; WebAuthn is refused.
- Rulesets support the rule types land policy can enforce; the others are refused (§8).
- Audit `S3` sinks write to local directories; `s3://` endpoints are refused.
- Exported archives carry no release assets, reactions, review comments anchored to lines or
  timelines; the importer reads what is present.
- `ResolveGitSshKey` resolves deploy keys only (user keys belong to the identity service).
