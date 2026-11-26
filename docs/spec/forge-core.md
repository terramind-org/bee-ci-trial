# Forge core (`bee-forge`)

The Beehive framework every forge area builds on: the forge store, migrations, event
logs and webhooks, leases, checks, orgs and repos, the audit log, authorization, and the
tonic services for `OrgService`, `RepoService`, `LeaseService`, `CheckService`,
`EventService` and `AuditService`. Contract: SPEC §11, §12.2, §15.1–§15.4.

## 1. Layout

| path | owns |
|---|---|
| `src/store/` | `ForgeStore`/`ForgeTx` traits, memory, SQLite and Postgres backends, migrations, conformance suite, failing stores for tests |
| `src/module.rs` | `ForgeModule` declarations and the `Registry` |
| `src/orgs/` | orgs, members, roles, invitations, org settings, custom property definitions; `repos.rs`: repos and everything per repo |
| `src/events/` | event logs, subscriptions, retention (`mod.rs`), webhooks (`webhooks.rs`), HTTP delivery and signatures (`delivery.rs`), VCS event bridge (`bridge.rs`) |
| `src/leases/` | path leases |
| `src/checks/` | check runs, suites, annotations, commit statuses |
| `src/audit/` | hash-chained audit log |
| `src/outbox.rs` | relay from surface outboxes to the event log and audit chain |
| `src/auth.rs` | authentication and authorization |
| `src/hooks.rs` | interfaces to the rest of the system (principal directory, VCS provisioning, revision resolution, policy source) |
| `src/forge.rs` | wiring and background workers |
| `src/service/` | the six tonic services |
| `src/media/` | media shared by posts and evidence: kinds and sniffing, per-class limits, the pure byte check `inspect`, uploads, response headers and `Range` |
| `src/changes/`, `src/briefs/`, `src/work/`, `src/community/`, `src/enterprise/`, `src/ci/`, `src/packages/`, `src/security/`, `src/agents/` | extension points (§9); `briefs`: evidence and the brief checklist of change requests (`docs/spec/forge-briefs.md`) |

## 2. Store

Forge data never lives in the VCS `MetaStore`. It lives in its own database:

- SQLite: `SqlConfig::sqlite(dir)` / `sqlite:<dir>`; one file per surface,
  `forge-<surface>.db`, WAL mode.
- Postgres: `SqlConfig::postgres(url, schema)` / `postgres://…?schema=<name>`; default
  schema `bee_forge`; one bounded connection pool per surface. A transaction dropped
  without commit (every read-only one) is rolled back in the background and its connection
  returned to the pool; only a connection whose rollback fails is discarded
  (`SqlStore::connections_opened` counts connects).
- Memory: `MemoryStore`, one lock per surface.

### 2.1 Surfaces

A **surface** is a failure domain: it owns tables and a connection pool (SQL) or lock
(memory). A transaction is opened on one surface and may touch only that surface's tables
(`INTERNAL` otherwise). Core surfaces: `orgs`, `leases`, `checks`, `events`, `webhooks`,
`audit`. Each extension module reserves its own (`changes`, `briefs`, `work`, …). Pool sizes are
per surface (`SqlConfig::pool_sizes`).

### 2.2 Row shape

Every table has the same columns; a module declares tables with `TableDef` and needs no
backend code.

| column | type | meaning |
|---|---|---|
| `scope` | TEXT | partition key: org id, repo id, log key, or `""` for global |
| `id` | TEXT | row id in the scope (ULID or natural key) |
| `k1`, `k2` | TEXT NULL | secondary keys; `unique_k1`/`unique_k2` make them unique per scope |
| `n1` | BIGINT NULL | ordering number: sequence, expiry, time, deletion time |
| `version` | BIGINT | row version, starts at 1 |
| `body` | BLOB/BYTEA | the record, protobuf-encoded (a `bee.v1` message or a crate-private prost message) |

Primary key `(scope, id)`; indexes are declared per table (`Index::ScopeK1`, `K2`, `N1`, …).
Keys are ULIDs and 64-bit integers only. Protobuf bodies inherit the proto's additive-only
rule, so old rows keep decoding.

### 2.3 Store method conventions

1. **One method, one transaction.** `let mut tx = db.begin(SURFACE, cx).await?; … tx.commit().await`.
   Dropping a `Tx` rolls back.
2. **Every method takes `&Cx`.** It carries the actor, delegation chain, request id, source,
   policy decision and the **deadline**; pool waits, locks and statements are bounded by it
   (`DEADLINE_EXCEEDED`, retryable). Postgres also sets `statement_timeout`.
3. **Optimistic concurrency.** Updates are `tx.update(&T, &row)` with `row.version` = the
   version read; a stale write is `VERSION_MISMATCH` with the current version in the payload.
   RPCs with `expected_version` pass it through (0 = don't check).
4. **After any error, drop the transaction.** On Postgres a failed statement aborts the
   transaction; check existence with `get`/`scan` before inserting instead of catching
   `ALREADY_EXISTS`.
5. **Emit, don't call.** Changes record their events with `tx.emit_event(Event{…})` and
   their audit entry with `tx.audit(AuditDraft::new(action, org_id)…)` in the same
   transaction. Both go to the surface's outbox; the relay moves them on. Never write to
   another surface inside a transaction.
6. **Reads honour soft state.** Expired leases, deleted repos and orgs, expired invitations
   are filtered when read, before any sweeper removes them.
7. Lists are served from the surface's own indexed tables and paged by an opaque token (the
   last id or sequence); search is never a dependency of a list.

### 2.4 Migrations

- Declared as data per module: `Migration { module, version, name, phase, ops }`, id
  `<module>/<NNNN>_<name>`, numbered from 1 without gaps, snake_case.
- Ops: `CreateTable(&TableDef)`, `CreateIndex(&TableDef, Index)`, `Sql { surface, sqlite, postgres }`.
- **Expand/contract.** `Phase::Expand` only adds and always runs. `Phase::Contract` removes
  and runs only with `SqlConfig::allow_contract` (after a release in which no code reads the
  removed thing); otherwise it is reported in `MigrationReport::pending_contract`.
- Applied migrations are recorded per surface in `forge_migrations` with a checksum; a
  changed migration with the same id refuses to start. Never edit an applied migration; add
  the next number.
- Table names start with `forge_` and are unique across modules (the registry checks).

### 2.5 Conformance

`bee_forge::store::conformance::run_all(store, prefix)` is the backend contract (CRUD and
versions, unique keys, rollback, scans and cursors, atomic counters, the outbox, surface
enforcement, pool isolation, migration idempotence and immutability). It runs against
memory and SQLite in `cargo test -p bee-forge`, and against Postgres when `BEE_TEST_PG`
is a connection URL.

## 3. Events

- Logs: one per repo (key = repo id) and one per org (`org:<id>`). Repo events are appended
  to both logs (separate sequences), so org subscribers and org webhooks see every repo.
- Sequences: strictly increasing per log, allocated in blocks (default 32) with one counter
  update per block; gaps are allowed. Appends to one log are serialized in-process (a cell
  owns its orgs), so sequence order is commit order.
- Dedupe: every append carries a key (the outbox id, or `vcs:<repo>:<seq>` from the bridge);
  a repeated key is a no-op.
- `Subscribe(from_seq)`: `0` = oldest retained; a sequence removed by retention is
  `INVALID_ARGUMENT`/`seq_not_retained`. Kind filters: exact, `prefix.*`, `*`; unknown kinds
  are `INVALID_ARGUMENT`/`unknown_event_kind`.
- Retention: 30 days by default (`ForgeConfig::event_retention_days`); the maintenance worker
  prunes.
- Body: typed (`ref_updated`, `change`, `review`, `check`, `lease`, …) where `bee.v1.Event`
  has a field, otherwise the subject as proto3-JSON in `payload_json` plus `subject`.
- Ordering across surfaces: the background relay runs one task per surface, so events of
  different surfaces are ordered by relay time; within a surface, commit order.
- VCS bridge (`events::bridge::VcsBridge`): reads the VCS store's per-repo events (payload =
  protobuf `bee.v1.Event`; a bare `Ref` is accepted for `ref.updated`) into the forge log.
  The dependency points forge → VCS only. A sync appends up to 500 events in one transaction
  that also advances the repo's cursor; syncs of one repo are serialized in-process.
- Listeners (`EventListener`): run inline after a single `EventLog::append` (domain events).
  Batched appends (`EventLog::append_batch`, the bridge) go to a bounded queue per listener
  (`LISTENER_QUEUE`) drained in order by that listener's own task, so subscribers and webhooks
  never wait on another module's reaction; `EventLog::settle` waits for the queues. A listener
  that does real work keeps it bounded itself: the security dependency-graph listener keeps
  the newest target per (repo, ref) for one worker and re-evaluates from the paths changed
  since its stored graph (`SecurityVcs::changed_paths`), listing a tree only for a repo's
  first graph.

### 3.1 Event kinds registry

Each module lists its kinds in `MODULE.event_kinds` (name, description, optional GitHub
event name and action). The registry rejects duplicates across modules. Naming:
`<subject>.<past-tense verb>` (`lease.acquired`, `repo.renamed`). Core kinds:

| module | kinds |
|---|---|
| events | `ref.updated`, `policy.changed`, `token.revoked` |
| orgs | `org.created/updated/renamed/deleted/restored/settings_updated`, `member.added/removed/role_changed`, `org_invitation.created/accepted/declined/cancelled`, `custom_property.updated`, `repo.created/updated/renamed/archived/unarchived/deleted/restored/transferred/settings_updated/topics_updated/properties_updated`, `collaborator.added/removed`, `repo_invitation.created/accepted/declined`, `autolink.created/deleted`, `resolver_hook.updated`, `mirror.created/deleted`, `deploy_key.created/deleted` |
| leases | `lease.acquired/renewed/released/expired` |
| checks | `check.updated`, `check_run.rerequested`, `check_suite.created/rerequested`, `status.created` |
| webhooks | `ping` |
| changes (reserved) | `cr.opened/revised/landed/closed/reopened/edited/ready_for_review/review_requested`, `review.submitted/dismissed`, `review_thread.resolved`, `merge_queue.entered/left`, `land_conflict.opened/resolved` |
| briefs | `evidence.added/updated/deleted` (subject: the change request; `cr.brief_set` is the changes module's) |
| work, community, enterprise, ci, packages, security, agents | see each module's `MODULE` |
| swarms | `swarm.created`, `swarm.state_changed`, `swarm.status_changed`, `swarm.task_added`, `swarm.task_started`, `swarm.task_finished`, `swarm.join_requested`, `swarm.join_decided` |

### 3.2 Webhooks

- Repo hooks (repo admin) and org hooks (org owner/admin), at most 20 per owner.
- Delivery: the dispatcher turns log events after a per-log cursor into jobs (same
  transaction advances the cursor); the bounded delivery pool (`DeliveryConfig::concurrency`)
  claims jobs with a version CAS, sends, logs each attempt, and reschedules failures with
  exponential backoff (`base * 2^(n-1)`, capped) up to `max_attempts`. At least once.
- Request: `POST`, JSON, `User-Agent: Beehive-Hookshot/1`, `X-Bee-Event`,
  `X-Bee-Delivery`, `X-Bee-Hook-ID`, `X-Bee-Signature-256: sha256=<hex HMAC-SHA256>`.
  `WEBHOOK_FORMAT_GITHUB`: GitHub payload shape (`action`, `repository`, `organization`,
  `sender`, subject under GitHub's key, plus `bee_event`) and `X-GitHub-Event`,
  `X-GitHub-Delivery`, `X-GitHub-Hook-ID`, `X-GitHub-Hook-Installation-Target-Type/-ID`,
  `X-Hub-Signature-256`. Kinds without a GitHub event are sent as `bee_<kind>`.
- `PingWebhook` and `RedeliverWebhook` deliver synchronously and return the logged
  delivery; redelivery re-sends the stored body with a new GUID and a fresh signature.

## 4. Leases (SPEC §11.5)

Overlap = same ref and component-wise path prefix either way (empty path = whole repo).
Exclusive leases of others block (`LEASED` with `lease_conflict`); advisory leases of others
are returned in `overlapping`; own leases never conflict. TTL default 2 h, max 24 h; renew
by holder; release by holder, or forced by a maintainer (`lease.break` audit action).
Expired leases are invisible at once and swept with `lease.expired`. `cr.landed` /
`cr.closed` events release leases tied to the change's ChangeIds (`leases::AutoRelease`).
The land path calls `Leases::land_check_fail_open`, which treats a forge outage as "no
leases" (SPEC §15.1); policy input: `Leases::policy_leases`.

## 5. Checks

Runs per changeset and name (reporting again creates a new run; "latest per name" is policy
input via `Checks::check_results`), annotations stored separately (≤ 1000 per call, ≤
10 000 per run), suites aggregated on read, rerequest = new queued run + event, commit
statuses mirrored onto `status:<context>` runs, `post_land` failures via
`Checks::failing_post_land`.

## 6. Audit

Every state-changing call writes an entry through its transaction's outbox; denied
state-changing calls are appended directly with `result` = the reason. Entry: actor, full
delegation chain and root (`on_behalf_of`), action, target, repo, org, request id, address,
user agent, token id, actor-supplied reason, `before`/`after` JSON, and the policy decision
in `after` as `{"policy_decision": "...", "state": …}`. One chain per org (`org:<id>`),
sequences without gaps, `hash = BLAKE3(prev_hash ‖ protobuf(entry without hash))`, genesis
`prev_hash` = 32 zero bytes. `VerifyAuditChain` reports `hash_mismatch`,
`prev_hash_mismatch`, `gap` or `head_mismatch`.

**Requirement for new modules:** every state-changing store method calls `tx.audit(...)`
with an action under one of the module's declared `audit_prefixes` (`<area>.<verb>`), and
every service method passes denials of state-changing calls through `guarded(...)`.

## 7. Authorization pattern

```rust
let caller = self.caller(&req)?;                  // verify Biscuit, build Cx
let repo = self.repo(&caller, &r.repo).await?;    // resolve (follows redirects)
let auth = self.forge.authz.repo(&caller, &repo, Action::Lease, &paths, Some(&ref_name), None).await;
let cx = self.guarded(&caller, &repo.org_id, &repo.id, "lease.acquire", auth).await?; // audits denials
if repo.archived { return Err(ForgeError::archived(&repo.full_name)); }
self.forge.leases.acquire(&cx, …).await          // store method with the authorized Cx
```

- Token: `bee_auth` verification, then the effective scope must allow the
  `bee_policy::Action` on the repo id, paths and ref.
- Roles: every principal of the chain (actor, then delegators) must hold a repo role
  (org owner/admin → admin; members → org base role; direct grants; public/internal read;
  registered `AccessSource`s) whose `bee_policy::builtin_role_actions` contain the action,
  plus an optional minimum role. No read access to a non-public repo → `NOT_FOUND`.
- Policy: when a `PolicySource` supplies the repo policy, `check_access_chain` per path.
- Org calls: `Authz::org(caller, org_id, OrgPerm::{ViewMembers, Manage, Own, ReadAudit, CreateRepo})`;
  `Manage`/`Own` need a token without a `repos` restriction that allows `manage_settings`.

| RPC group | action / role |
|---|---|
| repo reads, leases list/get, checks reads, events reads | `read` |
| lease acquire/renew/release-own | `lease` (write role) |
| break another's lease | `manage_settings`, maintain |
| report/update/rerequest checks, suites, statuses | `commit` (write role) |
| update repo metadata, topics | `manage_settings`, maintain (admin to change visibility) |
| settings, collaborators, autolinks, property values, resolver, mirrors, deploy keys, archive, delete, transfer, repo webhooks | `manage_settings`, admin |
| org profile, settings, members, invitations, property definitions, org webhooks, restore repo | org owner/admin |
| rename/delete/restore org, anything touching owners | org owner |
| audit log | org owner, admin, security manager |

## 8. Errors

Store and service code return `ForgeError` (a `bee.v1.ErrorReason`, message, snake_case
subcode, optional typed payload). `ForgeError::to_status(request_id)` builds the gRPC status
with the `ErrorDetail`; the code and `retryable` come from `bee_proto::errors`. Mapping:

| situation | reason / subcode |
|---|---|
| missing field | `INVALID_ARGUMENT` / `missing_field` |
| bad name, path, topic, URL, setting | `INVALID_ARGUMENT` / `invalid_name`, `invalid_path`, `invalid_topic`, `bad_url`, `invalid_setting`, … |
| unknown event kind filter | `INVALID_ARGUMENT` / `unknown_event_kind` |
| resume past retention | `INVALID_ARGUMENT` / `seq_not_retained` |
| absent or invisible | `NOT_FOUND` |
| name or key taken | `ALREADY_EXISTS` |
| stale `expected_version` | `VERSION_MISMATCH` + `version_mismatch` payload |
| exclusive lease conflict | `LEASED` + `lease_conflict` payload |
| wrong state (not deleted, last owner, invitation closed, not a template) | `INVALID_STATE` |
| archived repo | `ARCHIVED` |
| limit | `TOO_LARGE` + `limit_exceeded` |
| token / role / policy | `UNAUTHENTICATED`, `TOKEN_EXPIRED`, `TOKEN_REVOKED`, `POLICY_DENIED`, `PERMISSION_DENIED` |
| database down / busy | `UNAVAILABLE` (retryable) |
| deadline | `DEADLINE_EXCEEDED` (retryable) |
| served by another subsystem | `UNIMPLEMENTED` / `served_elsewhere` |

## 9. Extension points for later workers

A new area (for example change requests in `src/changes/`):

1. Fill in the module's `MODULE` (already registered in `Registry::core`): its surface,
   `TableDef`s, migrations `<module>/0001_…`, event kinds, audit prefixes.
2. Write the store as a struct over `Db` (`pub struct Changes { db: Db }`) following §2.3:
   one transaction per method, `&Cx` first, `tx.emit_event` + `tx.audit` for every change.
3. Add it to `Forge` (`src/forge.rs`) and, for listeners, `EventLog::add_listener`.
4. Implement the tonic service in `src/service/<area>.rs` following §7 and register it in
   `ForgeServices::register`.
5. Tests: the store against memory and SQLite (and Postgres via `BEE_TEST_PG`) through
   `tests/common` (`common::env()`), a service test with tokens of different scopes, and
   rows in `docs/coverage/tests/<task>.csv`.

Contracts other modules already rely on:

- `changes`: emit `cr.landed`/`cr.closed` with the `ChangeRequest` body (incl. `change_ids`)
  so leases auto-release; read `Checks::check_results` and `Leases::policy_leases` for
  `bee_policy::LandContext`; serve `RepoService.GetContext`.
- `enterprise`: teams/apps implement `orgs::AccessSource`; SIEM sinks and audit exports;
  enterprise audit logs (`enterprise:<id>` scope already supported by the chain).
- `ci`: report workflow jobs as check runs (`workflow_job_id`); bisect from
  `Checks::failing_post_land`; serve the bisect RPCs.
- The server wires a VCS-backed `RepoProvisioner` (root changeset, README/license, template
  copy), `RevisionResolver` and `PolicySource`, and runs `VcsBridge::sync_repo`.

## 10. RPCs served elsewhere

Answer `UNIMPLEMENTED` / `served_elsewhere` with the owner in the message:

| RPC | owner |
|---|---|
| `RepoService.ForkRepo`, `ListForks`, `SyncFork` | forks (phase 3, SPEC §11.1) |
| `RepoService.GetContext` | changes |
| `RepoService.TransferRepo` with `team_ids` | enterprise (teams) |
| `CheckService.StartBisect`, `GetBisect`, `ListBisects` | ci |
| `AuditService` sinks, exports, enterprise logs | enterprise |

## 11. API gaps

- `AuditEntry` has no field for the policy decision; it is carried in `after` as
  `{"policy_decision": …, "state": …}`. Proposed: `string policy_decision = 24;`.
- The policy action set has no check-reporting action; check writes require `commit`.
  Proposed: an action `report_checks` (and a `checks` token dimension).
- `Lease` has no `org_id` and no `created_at`; the store keeps the org next to the lease.
- `Webhook.insecure_ssl` is stored, but deliveries always verify certificates.
- `Event` has no typed body for org events or webhook pings; they use `payload_json`.
- `ListChecksRequest` and `ListCheckSuitesRequest` have no page; lists return at most 1000.
- `UpdateRepoRequest.description` is not `optional`, so it cannot be cleared.
- `GetCollaboratorPermissionRequest`/`CollaboratorPermission` cannot express custom roles'
  action sets; only built-in roles are evaluated here.
