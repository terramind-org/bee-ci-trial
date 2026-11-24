# Gap audit: our spec v0.2 vs the Anthropic post and the Scion reference

Generated 2026-10-17. Input for spec v0.3. Every item here must end up fully specified (no deferral).

I compared our spec against both sources and found a gap in every area you listed. **None of the protocol or ops contracts Scion treats as Core Conformance are in our spec**: config files and defaults, metrics, health endpoints, the audit log, backups, the full redaction lifecycle, the CLI command set with JSON schemas, and Scion's error-code table. Every feature you asked to stop deferring is either deferred to phase 2 or 3 in our PLAN.md, or not mentioned at all. That covers the regional cache, daemon snapshots, virtual checkouts, watcher-based status, copy detection, landing trains, bisect/revert and code search.

**How I read the sources.** The article only came back through WebFetch's summariser, so I worked from its summaries and the exact sentences it quoted for the terms I asked about; I never saw the raw page. In Scion I read all of SPEC.md, README.md, the docs and contract files, the proto, the schema list and the CLI argument definitions. I skimmed the section headings of `docs/PLAN.md` and read in full the parts not already copied into `docs/implementation-defined.md`. **One addition to your scope:** I also covered Scion's git gateway addendum (`docs/git-gateway.md`). It is a full contract for how git clients talk to a Scion server, and our §17.4 promises git compatibility without most of that detail.

**Source labels.**
- A = the article.
- B §x = Scion `SPEC.md`.
- G = `docs/git-gateway.md`.
- ID = `docs/implementation-defined.md`.
- SS = `docs/contracts/storage-semantics.md`.
- LD = `docs/contracts/line-diff3-v1.md`.
- "Ours §x" = `SPEC.md`.

**Status words.**
- MISSING = not in our spec.
- PARTIAL = mentioned but weaker or less detailed.
- DEFERRED = our spec or PLAN.md pushes it to a later phase or lists it as an open question.
- OK = covered.

---

## 1. Object model and encoding

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Max length of one path component | B §4.4: 1–255 bytes | MISSING (§4.2/§4.5 limit the whole path to 4096 bytes and 256 components only) | A 255-byte name limit, plus a validation subcode |
| Text form for derived object kinds | ID 6.6: `x1<hex>`, `x1<kk><hex>` | MISSING (§3.1 reserves kinds 0x10–0x7F but gives no letter) | A letter and text form for derived kinds |
| Ambiguous id prefix | B §19.1, ID 6.6: reject with a list of candidates (`ambiguous_prefix`) | MISSING | The error, its payload, and the rule that an exact ref name beats a hex prefix |
| Revision grammar | B §11.8: `~n`, `^k`, `git:<sha>`, `ref@version`; bare `~` = 1; `^0` invalid | PARTIAL (only the comment on `Revision.expr` in common.proto) | A full grammar section |
| History node object | B §9.2: kind 0x10, fields kind/content/changeset/parents/children | PARTIAL (§8 says "per-path history nodes" with no format) | The encoding, the kind byte, and the node-reuse rule |
| Global invariants list | B §4.5: four invariants (bytes match id; changeset row only if all dependencies exist; nothing mutated except listed state; ids never change) | PARTIAL (Invariant D plus scattered rules) | One consolidated invariants section |
| CreateChangeset validation order and subcodes | B §8.1: 8 ordered steps; subcodes `missing_blob`, `size_mismatch`, `noop_put`, `delete_not_file`, `file_directory`, `root_tree_mismatch`, path-rule violations, limits | PARTIAL (§7.2 has Invariant D and `MISSING_OBJECT` only) | The ordered steps and the subcode list |
| `expected_root_tree` check | B §7.5, §8.1 | MISSING (not in spec or proto) | The field and its mismatch error |
| Identical CreateChangeset twice returns one id with no error | B §8.1 | PARTIAL (§7 says "same inputs give same ids") | State it explicitly, including the concurrent case |
| Reference tree-building (apply-delta) algorithm | B §18.2 | MISSING | Pseudocode |
| Root changeset bootstrap | ID 6.5: exact author, time, creator, message and change_id; `main` created at version 1 via create-if-absent | PARTIAL (§4.4 says "fixed meta" without defining it) | The exact values |
| Very large directories | B App. A: a new tree format with sharded entries | MISSING | A design for sharded trees |
| Chunked large blobs | B App. A: BLAKE3 tree hashing allows chunking without changing ids | DEFERRED (§4.1 "reserved for a later format version") | Full chunk format and verified range reads |
| Line endings | B App. A: attribute-driven filters later | MISSING | A policy on line endings |
| One-way-door review | B App. C | MISSING | An equivalent appendix |
| Decision records | B App. B (D-1 to D-12) | MISSING | A decision-record appendix |

## 2. Storage

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Object-storage key layout | B §5.1, SS §7: `<prefix>/r/<repo>/<letter>/<hh>/<hex>`, plus a `q/` prefix for quarantine | MISSING | The key layout, including quarantine keys |
| Full metadata schema | B §5.2, SS §2: 14 tables (`objects` with state, `object_uploads`, `changeset_children`, `derive_leases`, `redactions`, `counters`, `audit_log`, …) and secondary indexes | MISSING (§6 names only `objects`, `object_paths`, `landed`) | The full schema and indexes |
| Per-object state live/quarantined/purged | B §5.2 | MISSING | A state column and the transitions between states |
| Upload records | B §5.2, §12.5: `object_uploads` table | PARTIAL (§9.3 "readable only by its uploader") | The table and the `HasObjects` rule that depends on it |
| zstd level setting | B §6.1: default 3 | MISSING | A config key |
| One store method = one transaction; retry rules | SS §1, §3: serialization failures (SQLSTATE 40001) retried up to 10 times with jitter; error subcodes `store_unavailable` / `txn_retry` | PARTIAL (§6.1 "one transaction"; no retry rules) | Retry rules for Postgres serialization failures |
| Changesets stored only in their metadata row | SS §7 | MISSING | State where changesets live |
| fsck violation list | SS §8 | PARTIAL (§14.5 lists what fsck checks) | The enumerated violation kinds |
| Decoded-object caches that keep serving during a metadata outage | SS §7, PLAN 7.2 | PARTIAL (§15.1 says reads survive) | Specify the caches |
| Backups | B §13.8, ID 6.16: document retention; at most 30 days | MISSING | Backup and restore policy, and a disaster-recovery test |
| GC of orphans and drafts | B App. A defers it | OK (§6.4) | Add a GC dry-run and safety window in the proto (`RunGc` exists) |

## 3. Write path

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Land states and attempt limits | B §8.4, §18.4: named states; 32 attempts; backoff 5–500 ms | PARTIAL (§7.4 says "the repo's attempt budget", no numbers) | Defaults and the state names |
| Error when land attempts run out | ID 6.7: `UNAVAILABLE` / `land_contention`, retryable | MISSING | The reason code |
| Land with `allow_empty` | ID 6.14: a member that rebases to nothing is recorded in the landed index with result = the current tip | MISSING (the proto field exists) | The semantics |
| Rebase of a multi-parent changeset | ID 6.7: `multi_parent` error | MISSING | The error |
| Merge changeset inside a land stack | ID 6.7: `merge_in_stack` | PARTIAL (§7.4 rejects it, with no reason code) | The reason code |
| Fast-forward rule on UpdateRef | B §8.5: `allow_non_fast_forward` flag, `force` permission, `NOT_FAST_FORWARD` error | MISSING (§7.5 plain CAS only; flag not in proto) | Flag, permission and error |
| Ref name grammar | B §4.3.6: regex, no `..` or `//` | MISSING | A grammar |
| ResolveRef at an old version | B §7.3: `at_version` | MISSING in proto (`name@version` exists only as prose) | The field |
| CreateRef/DeleteRef reason (for audit) and permission | B §7.6, scion.proto | MISSING | `reason` field; permission to create/delete refs |
| Reference algorithms for rebase and land | B §18.3, §18.4 | PARTIAL | Pseudocode |
| Merging stacks that contain merges | B App. A defers it | MISSING | A generalised stack walk |

## 4. Merge

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Normative line-merge contract with worked examples | LD: exact Myers trace, 12 diff examples (D1–D12), 12 merge examples (M1–M12) | PARTIAL (`docs/contracts/diff3-v1.md` is a stub) | The full contract and frozen golden examples |
| "Touching" hunk rule | LD §6: an empty insertion at or inside another range counts as touching | PARTIAL (§7.3.1) | The exact definition |
| Conflict marker rendering | B §11.7, LD §8: exact marker lines; insert `\n` before a marker if the preceding text lacks one | MISSING (§7.3.2 only "markers in a working copy") | Exact marker format |
| Line origins and unified hunks | LD §9 | MISSING | The format |
| Binary-detection boundary | B §19.4: NUL at byte 7999 is binary, at byte 8000 is text | PARTIAL (rule stated; boundary not) | The boundary test |
| Merge-engine version in rebase determinism | B §8.2: rebase is a pure function of (C, X, engine version) | PARTIAL | State the versioning |
| Rename/copy-aware merge | B App. A defers it | DEFERRED (our App. A.3) | A full design |
| Merges with several best bases (criss-cross) | B App. A: virtual base later | DEFERRED (our App. A.4 rejects in v1) | Recursive/virtual base algorithm |
| Add/add with different content and no base | B App. A: two-way merge later | MISSING | Rule |
| Differential test against `git merge-file` | B §19.4 | OK (§14.8) | — |

## 5. Derived data

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Generic derived-data RPC | B `GetDerived`, §19.6 | MISSING (no RPC) | The RPC |
| Content-bearing flag; purge deletes those records | B §9.1, §13.5, ID App. W3 | MISSING | The flag, keying by source object, and purge behaviour |
| Old type versions readable until GC | B §9.1 | MISSING | State it |
| Lazy derivation is iterative, never recursive | B §9.1 | PARTIAL | State it |
| Derivation leases | B §6.1: 30 s TTL, 16 lazy workers | PARTIAL (lease mentioned, no table or numbers) | Table, config and defaults |
| Blame algorithm | B §9.2: attribute lines parent by parent; redacted content treated as empty | MISSING | The algorithm |
| Path index rows for new trees at their directory path | B §9.2 | PARTIAL | Include trees |
| Log continuing through delete and re-add | B App. A | MISSING | Link history across deletion |
| Copy-detection index | A: "new indices for performing copy detection" | MISSING | A derived type, query RPC, and merge use |
| Path-based file search index / code search | A | DEFERRED (PLAN phase 3, "code search") | Full spec |

## 6. Fetch and cache tiers

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Regional cache servers (`scion-cache`) streaming from memory | A; B §2.3 and App. A defer it | DEFERRED (PLAN phase 3) | Full tier: topology, memory cache, sharding, same RPCs, config, metrics |
| Daemon starts from a periodically refreshed snapshot | A; B defers it | DEFERRED (PLAN phase 3) | Snapshot format, refresh cadence, how it is verified, fallback |
| Every tier above 99% hit rate; no long-lived state | A | MISSING | Targets for each tier as release gates |
| Serving stored zstd frames unchanged | B §7.3 | MISSING (our `ObjectChunk` carries uncompressed bytes only) | An encoding field (raw or zstd) |
| Range reads with offset and length | B §7.3 | PARTIAL (`resume_offset` only) | Offset and length fields |
| Per-item status (ok, not found, denied, redacted) | B §7.3 | PARTIAL (an `error_reason` string) | A status enum |
| Access class per object (root or restricted) | B §7.3, §10.4 | PARTIAL (concept in §10.2; no field) | `access_class` on objects and in CheckAccess |
| Object total size in the stream | B §7.3 | MISSING | A field |
| ServerInfo contents | B §7.3: protocol versions, both epochs, token public keys, limits, GitHub client id | PARTIAL (version, tier, algorithms, max blob only) | The missing fields |
| `scion-tier` response trailer | B §7.1 | MISSING (only epochs in §12.1) | The trailer |
| A batch must not fail because one item fails | B §7.1 | MISSING | The rule |
| Daemon cache layout, check on read, temp-file cleanup at startup | B §10.2 | PARTIAL | Specify them |
| Daemon upstream concurrency cap and queue | B §10.2: 256 concurrent, queue 4096, overflow = `RESOURCE_EXHAUSTED` | MISSING | Caps and the overflow error |
| Daemon retries upstream at most 3 times | B §10.5 | PARTIAL (general retry budget in §15.5) | Daemon-specific rule |
| Ref cache TTL | B §6.3: 1000 ms | PARTIAL (§10.2 "short TTL") | Default value |
| Access-decision cache TTL | B §6.3: 60 s; root-reader confirmation (ID 6.13) | PARTIAL | Values and the confirmation rule |
| Daemon redaction polling and fail-closed | B §13.3: poll every 10 s; stop serving from cache after 600 s stale | PARTIAL (§10.2 forwards everything if it cannot confirm it is current) | Polling, staleness bound, cached-copy deletion |
| `HasObjects` always forwarded by the daemon | ID 6.13 | MISSING | State it |
| Non-object RPCs forwarded byte-for-byte | ID 6.13 | PARTIAL | State it |
| A tier talks only to the tier directly above | B §3.3, §19.8 | MISSING | Rule plus a network-policy test |
| Daemon socket open to all local users; auth by token | B §17 | MISSING | State it |
| Daemon metrics and health listener | SS amendment S-5 | MISSING | Listener and endpoints |
| Daemon stats | PLAN 2.16: hits, misses, fail-closed, epochs | MISSING | A stats surface |
| Fetch reads in batches of 256 objects; objects verified before writing | B §10.1 | PARTIAL (§10.1) | Batch default |
| API-only use with no checkout | A | OK (§1.1) | — |

## 7. Working copy and CLI behaviour

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Virtualized checkouts (files materialised on demand) | A; B defers (FUSE on Linux; NFS or FSKit on macOS) | DEFERRED (PLAN phase 3) | Full spec |
| `status` backed by a filesystem watcher | A (implied); B defers | MISSING | Spec, with the stat-cache walk as fallback |
| `.bee/` layout: config, state, index, conflicts dir, lock | B §11.1 | PARTIAL (§10.3 lists contents; no lock, no conflicts dir) | The full layout |
| File formats: state as canonical TOML; versioned binary index | ID 6.15 | MISSING | Formats |
| Advisory lock: one mutating command at a time | B §11.1 | MISSING | The lock |
| Sparse add/remove semantics | B §11.2: refuse dirty files without `--force`; directories on the way are created | PARTIAL | Semantics |
| `goto` | B §11.3: diff old and new trees within scope, preserve unrelated dirty files | MISSING | The command |
| `.gitignore` rules (nested files, negation) | B §11.4 | MISSING | Ignore semantics |
| Status categories | B §11.5: modified, added, deleted, mode_changed (plus redacted_dirty) | PARTIAL | Categories |
| Commit and amend semantics | B §11.6: no staging area; `--path`; amend reuses parents and ChangeId | PARTIAL | Semantics |
| Rebase conflict workflow, `resolve`, `rebase --abort` | B §11.7 | MISSING | Workflow |
| Writes use `openat` with no-follow | B §11.3 | OK (§10.3) | — |
| Case-collision detection forced on, deterministic | ID 6.17 | PARTIAL | Detection rule |
| Client config file, credentials file mode 0600, env overrides | B §6.4, §17 | MISSING | All three |
| CLI exit codes | ID 6.15: 0 ok, 1 error, 2 usage, 3 conflicts, 4 dirty refusal | MISSING | Exit codes |
| JSON envelope | ID 6.15: `{"schema":…, "data":…}` | PARTIAL (schemas/v1 README) | The envelope |
| Unified diff output format | ID 6.15 | MISSING | The format |

## 8. Auth, policy and redaction

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Signing-key rotation | B §12.1: several keys; first signs, all verify; key id | MISSING (Biscuit, nothing on rotation) | Rotation |
| Default token lifetime | B: 12 h | MISSING | Default |
| Service secrets | B §12.3: stored as SHA-256, compared in constant time, never logged | PARTIAL (§9.1) | The rules |
| Login by exchanging a GitHub token | B §12.2: client runs the device flow with GitHub, then sends the GitHub token to Login | PARTIAL (we run the device flow through the server) | Pick one and specify it |
| What happens when GitHub is down | B §16.1: existing tokens keep working; new logins fail | MISSING | Rule |
| Daemon CheckAccess by object id | B §7.7: takes a subject token and object ids; returns allowed + access class | MISSING (ours takes paths and an action) | A daemon variant |
| WhoAmI groups and expiry | B §7.7 | MISSING | Fields |
| Policy prefix matching | B §6.2: component-wise (`/secrets` does not match `secrets2`); a `/` rule is required | MISSING | Both rules |
| Nested groups and cycle detection | B §6.2 | PARTIAL ("groups") | Rules |
| Ref-pattern precedence | B §6.2: exact name beats pattern; longest pattern wins | MISSING | Rules |
| Permissions to redact, purge, manage refs and force-update | B §6.2 | MISSING | Add to policy |
| Invalid policy | B §6.2: keep the last good policy; fail at startup | PARTIAL (policy lives in the repo) | Behaviour when a policy change lands invalid; validate at land |
| How the epoch is computed and its trailer format | ID 6.2: first 8 bytes of BLAKE3 over the normalised policy; decimal u64 | PARTIAL | Exact derivation |
| Required read permission for Rebase, IsAncestor, MergeBase, GetDerived and Resolve; ListRedactions open to any valid token | ID 6.12 | MISSING | Authorization table for every RPC |
| History filtering in Log and Diff without a path | B §12.6 | PARTIAL | Rules |
| Meta of changesets that touch only restricted paths | B App. A: per-changeset visibility | MISSING | Visibility rule |
| Hiding restricted names / redacting path names (masking at serve time) | B App. A, C | MISSING | Masking with server-signed views |
| GitHub team-based groups | B App. A | MISSING | Group-source adapter |
| Optional mutual TLS | B §17 | MISSING | State it |
| Redaction lifecycle: Live → Quarantined → Purged | B §13.2 | MISSING (§13 is one paragraph) | Lifecycle |
| Redaction details: default quarantine 7 d; move bytes to quarantine; Unredact only from quarantine; PurgeNow; scheduled purge | B §13.2 | MISSING (`purge_now` flag only; no `purge_after`) | All of it; RPC fields |
| Propagation bound | B §13.3: 1000 ms; response carries `effective_by` | MISSING | Bound and field |
| Rule for consumers when an unredaction happens | SS §4.5: re-list from epoch 0 | MISSING | Rule |
| How reads look for redacted objects | B §13.4: log/show render `<redacted>`; integrity check skipped | PARTIAL | Rules |
| Redacted entries carried forward unchanged stay valid | B §13.6 | MISSING | Rule |
| Placeholder text; newly redacted clean files replaced on next mutating command; dirty files reported | B §13.7 | PARTIAL ("fixed placeholder") | Exact bytes and the refresh rule |
| Redaction tooling | B §13.2: `--path --at --all-versions`, `--object`, `--meta` | MISSING | CLI |
| Error choices | ID 6.7: `not_quarantined`, `not_redactable`, NOT_FOUND | MISSING | Errors |

## 9. Admin, ops and configuration

| Item | Source says | Ours | What to add |
|---|---|---|---|
| TOML configs; unknown keys rejected; `$VAR` resolution | B §6 | MISSING (only `--dev`) | Config files |
| Server config sections | B §6.1: server, metadata, objects, auth, policy, limits, land, derive, redaction, observability; plus rate_limit and object-store region (ID 6.11) | MISSING | Every key |
| Daemon config | B §6.3: 11 keys | MISSING | Every key |
| Defaults table | B §6.5 | MISSING | Table |
| Audit log | ID 6.8, SS §4.6: table; written in the same transaction as ref create/delete, forced updates, redact/unredact/purge | MISSING | Table and an RPC to read it |
| Health endpoints `/healthz` and `/readyz` | B §15.3 | MISSING | Endpoints |
| Integrity alerting | ID 6.10 | MISSING | Alert event and counter |
| Repo id derivation | SS 1.5 | n/a (we use ULIDs) | — |
| Operational validation before production | B §20.3 | MISSING | Checklist |
| Rolling upgrade under load | B §19.19 | PARTIAL (§15.2 cell-by-cell rollout) | A test |

## 10. Observability and metrics

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Required structured-log fields | B §15.1: request_id, rpc, tier, principal, repo, status, duration_ms, changeset_id, ref, ref_version, land_attempt | MISSING | Field list |
| Never log raw paths, secrets or contents | ID 6.9: log a path hash instead | MISSING | Rule |
| Server and daemon metrics | B §15.2: `scion_rpc_duration_seconds`, `cache_requests_total`, `cache_bytes`, `land_attempts`, `ref_cas_failures_total`, `land_duration_seconds`, `derive_duration_seconds`, `derive_backlog`, `redaction_epoch`, `redaction_lag_seconds`, `policy_epoch_info`, `object_bytes_served_total`, `upstream_requests_total`; plus `integrity_failures_total`, `storage_access`, `storage_bytes` (ID, README) | MISSING | Prefixed metric list |
| Git gateway metrics | G §15: eight metrics | MISSING | List |
| OpenTelemetry tracing | B recommends; Scion's plan defers | MISSING | Spec |
| "Superfans": Claude instances that each watch one component through telemetry and logs | A | MISSING | An agent-facing telemetry and log access API, anomaly feed and per-component scope |

## 11. Rate limiting

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Per-principal limits | ID 6.11: 500 rps, burst 1000, 4096 in flight | PARTIAL (§15.4 has tiers and quotas, no numbers) | Defaults |
| Bulk reads cost more | SS S-5: GetObjects/HasObjects charge one extra token per 1000 ids | MISSING | Cost model |
| Shed load before dependencies saturate | B §10.5 | OK (§15.4) | — |
| "Sophisticated rate limiting" | A | PARTIAL | Algorithm (token bucket or other) |

## 12. Testing

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Fuzzing | A | MISSING in both | Fuzz targets: decoders, merge, pack parsing, policy parsing |
| Validation profiles | B §19: core, extension, scale/resilience | MISSING | Profiles |
| Scale and resilience runs | B §19.19: 72 h land throughput; 14-day soak with faults; database node and zone loss; disaster-recovery restore; rolling upgrade; above 99% hit rate; largest import | PARTIAL (§16 targets) | Each run |
| Detailed test matrix | B §19.1–19.17: 10⁶ round trips; inline-threshold ±1; binary boundary; CRLF; ACL matrix (every RPC × server/daemon); retroactivity; existence probing; config; log scan; every failure class; crash at every client step; randomised status edits; gitignore vs `git check-ignore` | PARTIAL (§14) | The matrix |
| Test infrastructure | FakeGitHub, FakeUpstream, history recorder, env vars for case counts, layer-check self-test | PARTIAL | Name each |
| Definition-of-done checklist | B §20 | MISSING | Checklist |
| Git gateway tests | G §16: projection golden vectors; byte-identical round trip; object-set completeness; client matrix | PARTIAL (§14.8) | Suite |

## 13. Migration and import

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Importer as an RPC mode | ID 6.4: `import_mode`, needs ref-admin and write on `/` | MISSING | Field |
| Importer runs as a service principal with those rights | B §14 | MISSING | Rule |
| Importer flags and incremental re-import | B §14 | PARTIAL (§17 is GitHub-level) | Flags |
| Commit-graph equality after re-import | README | PARTIAL | Test |

## 14. Review tool ("Antique" semantics)

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Rich semantic approval constraints; directory permissions | A | OK (§9.4, §11.3) | — |
| Code search | A | DEFERRED (phase 3) | Full spec |
| Issues RPC | — | PARTIAL (§11.8 in spec; no service in proto) | IssueService |
| Releases RPC | — | PARTIAL (§17.2 mapping only) | ReleaseService |
| Repo context RPC | — | `GetContext` is in the proto but not in the spec | Spec it |

## 15. Merge queues

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Antique has a merge queue (it has its own superfan), yet no global merge-queue lock | A | DEFERRED (§7.4 "landing trains (phase 2)") | Queue states, ordering, fairness, events, RPCs, metrics |

## 16. Bisect, revert and post-merge testing

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Automated bisect and revert pipelines | A | DONE (§11.6; `docs/spec/workflows.md` §13): confirm re-run, bisect, culprit CR mapped mid-stack, revert with `auto_revert` | — |
| Pre-deploy testing | A | DONE (§11.6): `green/<ref>` written forward-only by CI; `require_green` gates API and workflow deploys; `deploy_green_tip` | — |
| Post-merge testing | A | DONE (§11.6): `post_land` checks define green; opt-in `merge_queue_check_combined` tests the combined result before landing | Parallel speculative queue builds |

## 17. Monitoring, failure model and protocol rules

| Item | Source says | Ours | What to add |
|---|---|---|---|
| Ten failure classes and recovery invariants | B §16 | PARTIAL (§15) | Full list |
| Breaking changes ship as a new package served side by side; unknown fields ignored; missing required fields give `missing_field` | B §7.1, §19.7 | MISSING | Rules |
| Error-code table | B §7.2: NOT_FOUND, PERMISSION_DENIED, UNAUTHENTICATED, INVALID_ARGUMENT with subcode, TOO_LARGE with limit payload, INTEGRITY, REDACTED, REF_VERSION_MISMATCH with current version/target, NOT_FAST_FORWARD, RESOURCE_EXHAUSTED, UNAVAILABLE — each mapped to a gRPC status and a retryable flag | PARTIAL (§12.2 lists reasons with no mapping; payloads only for conflicts, leases, policy) | Table and payloads |
| Git gateway contract (whole addendum) | G | PARTIAL (§17.4) | Native commit format with `scion-change-id`; git tree ordering; author-name sanitising; collision-detecting SHA-1; one object format per repo; protocol v2 capabilities; minimum git 2.30; negotiation by generation numbers; object-set algorithm; `blob:limit`, `tree:<n>`, `object-info`; omission rule; seed packs via packfile-uris/bundle-uri; delta hints and thin packs; receive-pack capabilities; push ingest checks; change-id trailer and commit-msg hook; `refs/scion/c/*` virtual refs; replace refs for redacted commits; `[git]` gates and `git = false`; record git ids before quarantine; preflight; credential helper; HTTP Basic with token; error mapping to `ng`/`ERR`; the five gateway RPCs; git storage tables; gateway config; tags; atomic push; SSH non-goal; LFS serving |
| Cells, forks, Actions compatibility | — | DEFERRED (§15.2, §11.1, App. A.5–A.7, PLAN phase 3) | Specify fully, if zero deferral includes our own items |

---

## Scion CLI commands

Our spec has no command list, and `schemas/v1` holds only `README.md` and `error.schema.json`. The only commands it names are `import github`, `mcp`, `daemon`, `checkout` and `land`. Every Scion command below therefore has no counterpart in our spec or schemas.

| Command (schema file) | Flags | In ours |
|---|---|---|
| `login` | `--service NAME` (secret from env or stdin), `--no-store-github` | MISSING |
| `whoami` | — | MISSING |
| `checkout <rev> [dir]` | `--sparse <prefix>…` | PARTIAL (named only) |
| `sparse add <prefix>…` / `sparse remove <prefix>…` | `--force` | MISSING |
| `status` | — | MISSING |
| `diff` | `--name-only`, `-U/--unified N` | MISSING |
| `commit` | `-m`, `--path` | MISSING |
| `amend` | `-m` | MISSING |
| `goto <rev>` | `--force` | MISSING |
| `rebase` | `--onto <rev>`, `--abort` | MISSING |
| `resolve <path>` | `--force` | MISSING |
| `land` | `--ref` | PARTIAL (named only) |
| `log` | `--path`, `--from`, `--first-parent`, `-n`, `--oneline` | MISSING |
| `blame <path>` | `--at` | MISSING |
| `show [rev]` | — | MISSING |
| `cat <rev>:<path>` | — | MISSING |
| `redact` | `--path --at [--all-versions]`, `--object`, `--meta`, `--reason`, `--purge-after`, `--now` | MISSING |
| `error.schema.json` | — | OK |

- **Global flags:** `--json`, `--endpoint`, `--ca-file`, `-C DIR` are all MISSING.
- **Git gateway commands:** `git credential`, `git setup`, `git preflight` are MISSING.
- **Binaries:** `scion-server --config`, `scion-daemon --config` and `scion-import --git --ref --target-ref --skip-submodules` have no counterpart for config or import flags in ours.

## Scion RPCs compared with our proto

| Scion service | RPC | In our proto |
|---|---|---|
| Object | GetServerInfo | Present, missing fields (§6) |
| Object | GetObjects | Present, missing encoding, status, access class, total size, offset/length |
| Object | HasObjects | Present |
| Object | ResolveRef | Present, missing `at_version` |
| Query | GetChangesets | Present, missing per-item status |
| Query | ReadPath, ListTree, Log, Blame, Diff, IsAncestor, MergeBase, Resolve | Present |
| Query | **GetDerived** | **None** |
| Write | UploadObjects, Rebase, Land | Present |
| Write | CreateChangeset | Present, missing `expected_root_tree`, committer, extensions, `import_mode` |
| Write | Merge | Present, missing meta fields |
| Ref | UpdateRef | Present, missing `allow_non_fast_forward` |
| Ref | CreateRef, DeleteRef | Present, missing `reason` |
| Ref | GetRefLog | Present |
| Auth | Login | Present (different flow) |
| Auth | WhoAmI | Present, missing groups and expiry |
| Auth | CheckAccess | Present but different meaning: **the daemon's by-object-id form has no counterpart** |
| Admin | Redact | Present, missing `purge_after` and `effective_by` |
| Admin | Unredact | Present |
| Admin | **PurgeNow** | **None** (only a `purge_now` flag on Redact) |
| Admin | ListRedactions | Present, missing `state` |
| Git (G) | **GetGitObjects, GitListRefs, GitPlanFetch, GitResolve, GitIngestCommits** | **None** |

Our proto also has things Scion lacks: `ListRefs`, `RunGc`, `CreateAgent`, `MintToken`, `Revoke`, and the Org, Repo, Change, Check, Lease, Event and Import services. On the other side, our spec's §12.3 lists an Admin "purge" that the proto doesn't implement, and neither the spec nor the proto has an Issue or Release service.