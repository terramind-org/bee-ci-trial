# Security (`bee-forge::security`)

Status: implemented in `crates/bee-forge/src/security/`. Serves `SecurityService`
(`proto/bee/v1/security.proto`). Scope: PRODUCT §4.18 (scenarios AS-SEC-01, AS-SEC-02; real-world
check 6). The libraries are `bee-secrets` (detection) and `bee-deps` (graphs, OSV matching,
update planning, SBOM; see `docs/spec/security-deps.md`). This module is the forge side: storage,
state machines, authorization, events and audit. Conventions: `docs/spec/forge-core.md` §9.

## 1. Storage, events, audit

One surface, `security`; every state change is one transaction that also records its event and
audit entry. Audit prefixes `security.` and `push_protection.`. Records are protobuf (`bee.v1`
messages inside crate-private wrappers, `security/model.rs`); secrets are never stored, only a
redacted preview and a BLAKE3 hash keyed by the repo id.

Event kinds (GitHub names where one exists): `security_alert.created|fixed|dismissed|reopened|
auto_dismissed`, `dependency_snapshot.submitted`, `dependency_graph.updated`,
`dependency_update.run_started|run_finished|config_updated|change_opened`,
`secret_scanning_alert.created|resolved|reopened|validated`, `secret_scanning_pattern.created|
updated|deleted`, `push_protection.blocked|bypass_requested|bypass_reviewed|bypassed`,
`code_scanning_alert.created|fixed|reopened|closed_by_user`, `code_scanning.analysis_processed|
setup_updated|autofix_requested`, `code_quality_finding.updated`, `security_advisory.created|
updated|published|cve_requested|collaborator_changed|reported`, `security_config.updated`,
`security_campaign.created|updated`.

## 2. Authorization

| RPCs | needs |
|---|---|
| dependency graph, manifests, SBOM, dependents, dependency review, configuration read, signature verification | `read` |
| advisory database | any authenticated principal |
| snapshots, dependency and code scanning alerts and uploads, update config and runs (reads), code quality, bypass requests and reads | `commit` (write role) |
| secret alerts and locations, update config writes, update runs, default setup writes, advisory writes, CVE requests, advisory collaborators | `manage_settings` (maintain role) |
| repo patterns, validity checks, publish, private reporting switch, configuration writes | `manage_settings`, admin |
| bypass review | repo admin, org security manager (or owner, admin), or a reviewer listed in the repo's bypass policy; never the requester |
| org patterns, validity checks, overview, campaigns | org owner, admin or security manager |
| private vulnerability report | `read` (an outsider who can see the repo) |

An org security manager passes every repo level without a repo role. Denied state changes are
audited. Unpublished advisories are visible to maintainers, the author, the reporter and
collaborators; anyone else gets `NOT_FOUND`.

## 3. Dependency graph, SBOM, review

The graph of a revision is `bee-deps` over the dependency files of the tree (found by probing
paths with the detector) merged with submitted snapshots (those for exactly that changeset, and
the newest of the default branch at its tip). `SubmitDependencySnapshot` replaces by
`<detector>:<job_correlator>`; malformed purls are `INVALID_ARGUMENT/invalid_package_url`, more than
20 000 dependencies `TOO_LARGE`. The default branch graph is stored and re-parsed only when the
digest of its dependency files or snapshots changes (`on_default_branch_moved`, called by the
`ref.updated` listener and by snapshot submission). SBOMs are SPDX 2.3 / CycloneDX 1.6 JSON, inline
up to 16 MiB or by signed URL through the publisher port (always when asked, or above 1 MiB).
`ListDependents` lists public repos, and repos of the same org, whose graph holds a package the
repo publishes (package.json, Cargo.toml, go.mod, pyproject.toml names). Dependency review diffs
two graphs (a CR's last revision base and head, or two revisions) against the advisory index;
for a CR it reports a `dependency-review` check run (one per verdict per head).

## 4. Advisories and vulnerability alerts

The advisory index is OSV records from the feed port plus published repo advisories. A refresh
reloads when the feed version changes, diffs records by digest and re-evaluates only repos whose
stored graph holds a package of a changed, added or removed advisory. Alerts are keyed
`<purl>|<manifest>|<advisory id>`: new findings open alerts; a fixed alert whose finding returns
reopens; an open alert whose finding is gone is `FIXED`, or `AUTO_DISMISSED` (reason `INACCURATE`)
when its advisory was withdrawn or removed; dismissed alerts stay dismissed. Dismissing needs a
reason (`INVALID_ARGUMENT/missing_reason`); fixed alerts cannot change (`INVALID_STATE`).

## 5. Update runs and update change requests

Configuration is the repo's `.beehive/dependabot.yml`, else `.github/dependabot.yml` (the same
format; `groups`, `open-pull-requests-limit`, `target-branch`, `ignore`, `labels`, `directories`,
cron or interval schedules), plus server switches (`SetDependencyUpdateConfig`;
`security_updates_enabled` is the repo's security configuration). A run plans with
`bee_deps::update`, applies manifest edits, refreshes lockfiles (`TextLockRefresher`: package-lock
v1 to v3, Cargo.lock, go.sum, hashed requirements; integrity from the registry port, else the stale
value is removed or the file left alone), commits on the default branch tip as the bot principal
and opens a CR through `Changes::open`. One fix opens one CR (remembered per ecosystem, directory,
group and target versions); the open-CR limit per entry applies; grouping follows config groups and
`grouped_security_updates`. New alerts with security updates on start a run by themselves;
`run_due_updates` starts scheduled version updates (registry port). A queued or running run for the
same entry makes another `RESOURCE_EXHAUSTED/update_run_queued`. Runs execute inline; stuck
queued runs are executed by `maintain`.

## 6. Secret scanning and push protection

Built-in rules plus published custom patterns of the repo and org. A custom pattern is
`(?:before)(?P<secret>regex)(?:after)` with `must_match` / `must_not_match` applied to the secret;
a bad regex is `INVALID_ARGUMENT/invalid_regex`, a duplicate name `ALREADY_EXISTS`, at most 100
per scope. Alerts are keyed `<secret_type>:<keyed hash>`: one alert per distinct secret with every
location (blob path/line/changeset, the changeset being the oldest that introduced it, or an
issue/comment subject). Resolution reasons users may set: false positive, won't fix, revoked, used
in tests; `PATTERN_EDITED` and `PATTERN_DELETED` are set by editing or deleting a pattern, which
rechecks or resolves its alerts. Validity comes from the verifier port when checks are on (repo or
org switch) and is refreshed when an open alert is read.

`Security::protect(cx, repo, files, bypass_id)` is the single check the server calls at
CreateChangeset, Land and git push. Clean content passes. A blocking finding (medium or high
confidence, or a custom pattern with `push_protection`) fails with `SECRET_DETECTED` carrying a
`PushProtectionBlock` whose `bypass_request_url` ends in the block id (valid 24 h). Bypass policy
per repo (`set_bypass_policy`): `review` (default; a pending request, valid 7 days, decided by a
reviewer other than the requester, approved bypasses valid `ttl_hours`), `allow` (granted at once),
`deny`. A bypass is for its requester, the findings of its block, and one write; exercising it
opens alerts marked bypassed and is always audited (`push_protection.bypass`).

## 7. Code scanning

`UploadSarif` accepts gzip SARIF 2.1.0 (10 MiB, 100 MiB inflated, 20 runs, 25 000 results per run)
or a URL through the fetcher port. Alerts are keyed `<tool>|<rule>|<path>|<fingerprint>`
(`partialFingerprints`, else `fingerprints`, else a hash; duplicates within a run get `:n`).
Each (ref, category) holds one instance; an alert is open while any instance is open, fixed when
all are, and a fixed alert reopens when the finding returns. Suppressed results are ignored.
Results of rules tagged maintainability, reliability or quality (and not security) are code quality
findings. Each analysis publishes a check run `code-scanning/<tool>[/<category>]` (failed on
error-level or high severity results, neutral on others) with up to 1000 annotations. Default setup
is stored configuration. Autofix goes through the autofix port.

## 8. Repository advisories

Private reports (`TRIAGE`) need private vulnerability reporting; at most 10 open per reporter and
repo. States: TRIAGE to DRAFT or CLOSED; DRAFT to CLOSED; CLOSED to DRAFT; PUBLISHED to WITHDRAWN
(final). Drafts carry credits, collaborators, an optional private fork (port) and a CVE request
record (`fulfil_cve_request` records the assigned id). Publishing writes the global database, makes
the advisory matchable at once and raises alerts; edits and withdrawal follow into the global record.

## 9. Overview, campaigns, configuration

The overview counts open alerts by severity per repo and tool, fixed and dismissed inside the
window, coverage of features, and mean time to remediate. Campaigns are saved alert filters with
live progress (names are unique among active campaigns). Configuration: graph, alerts, security
updates, default setup, secret scanning, push protection, validity, private reporting, quality,
required dependency review; push protection needs secret scanning, alerts need the graph, updates
need alerts.

## 10. Maintenance

`Security::maintain` (the server's worker): pending SARIF, advisory reload with incremental
re-evaluation, due scheduled updates, expiry of blocks and bypass requests.

## 13. Ports the server must back

`SecurityVcs` (resolve, list files, read file, added blobs of a changeset, history; `HistorySecurityVcs`
over `bee-history` is provided), `AdvisoryFeed` (OSV records; `DirFeed` provided), `SecretVerifier`,
`Registry` (latest version, lock integrity), `LockRefresher` (optional replacement for the text
refresher), `UrlFetcher`, `DocumentPublisher` (signed SBOM URLs), `AutofixRunner` (agents),
`SignatureVerifier` (`VerifyObjectSignature`: bee-server installs `forge::facts::ServerSignatures`,
which verifies a changeset's git `gpgsig` against the signing-key registry and a tag annotation's
signature over its message), `AdvisoryForks`. The server also
calls `protect` at its three write paths, `scan_changeset` after writes, `on_default_branch_moved`
(or attaches the listener) and runs `maintain`. Construction: `Security::new(&forge, Some(changes), Ports)`,
`SecurityServices::new(forge, security).register(&mut routes)`.

## 14. API gaps

- `fulfil_cve_request` has no RPC (CNA integration); `CveRequest` has no wire message.
- Advisory collaborator teams are stored but not resolved to members for visibility.
- `SecurityConfiguration.inherited_from` is never set: no org-level default configuration.
- `ListCodeScanningAlertsRequest.states` empty means open (unspecified in the proto).
- `TriggerDependencyUpdateRun` returns the finished run (runs execute inline), not a queued one.
- `UpdateSecurityAdvisoryRequest` cannot change credits.
- `PushProtectionBypass.audit_entry_id` names the audit entry of the bypass's latest transition
  (request, review or use); earlier entries are found by querying the log for the bypass id.

Closed (2026-10-19): the bypass policy is part of `SecurityConfiguration`
(`push_protection_bypass_mode`, `bypass_reviewer_ids`, `bypass_ttl_hours`, set through
`UpdateSecurityConfiguration`), `RescanSecrets`, `GetSecurityPolicy` (SECURITY.md with a
sanitized rendering), and `PushProtectionBypass.audit_entry_id`.
