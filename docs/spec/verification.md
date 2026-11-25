# Verification: three-form conformance (H2)

PRODUCT §3.2 says every operation exists as the native API (gRPC and HTTP/JSON), as
`bee <noun> <verb> --json`, and as an MCP tool, and that the three behave identically. This
document describes the suite that checks it for every served RPC, and holds its latest
results.

Code: `crates/bee-conformance/src/parity/` (fixtures, templates, naming rules,
normalisation, report) and the runner `crates/bee-conformance/tests/three_forms/`.
Fixtures: `crates/bee-conformance/fixtures/`.

## 1. What is checked

For each RPC in the proto descriptor that the server serves (everything except the
UNIMPLEMENTED allow-list in `crates/bee-server/tests/forge_services.rs`, read from that file
so the two never drift):

1. **A fixture exists.** One fixture per served RPC, in `fixtures/<noun>.json`. A served RPC
   without a fixture, or a fixture for an RPC that is not served, fails
   `every_served_rpc_has_a_fixture`, so a new RPC cannot skip conformance.
2. **Names follow SPEC §12.4.** The CLI command is `bee <noun> <verb>` (service without
   `Service` and method, kebab-case, acronyms kept together), its output schema is
   `schemas/v1/cli/<noun>.<verb>.schema.json`, and the MCP tool is `<service>_<method>` in
   snake_case. The rules are implemented again in `parity::naming` rather than borrowed from the
   generators.
3. **`tools/list` matches the descriptor.** With the `all` toolset, `/mcp` lists a tool for
   every served RPC, and its `inputSchema` equals
   `schemas/v1/messages/<request>.schema.json` (server streams add `max_items` and
   `timeout_ms`; the client stream `UploadObjects` takes `{"messages": [...]}`). Each
   fixture's arguments are also validated against the tool's `inputSchema`.
4. **The three forms agree.** Each fixture's request is sent as
   - **API**: a raw HTTP client on the Connect surface (`POST /bee.v1.<S>/<M>`, unary or
     enveloped streams), checking that every error carries a `bee.v1.ErrorDetail` whose
     binary and debug forms agree and whose Connect code and HTTP status match the SPEC §12.2
     row for its reason. Reads (`Get*`, `List*`, `Search*`, `Resolve*`, `Log`, `Diff`, ...)
     are sent again over gRPC with a raw-bytes tonic client and must give the same outcome.
   - **CLI**: the `bee` program as a separate process,
     `bee --json --endpoint <url> --token <t> <noun> <verb> --<field>=<value> ...
     [--json-input <rest>]`. Top-level scalar and enum fields, and a `RepoRef` given by
     name, become flags; the rest goes in `--json-input`. Every document must validate
     against its `schemas/v1/cli/` file (errors against `schemas/v1/error.schema.json`) and
     the exit code must be 0, or 3 for `MERGE_CONFLICT`, 5 for `POLICY_DENIED`, `LEASED`,
     `SECRET_DETECTED` and `BUDGET_EXHAUSTED`, else 1.
   - **MCP**: `tools/call` on `/mcp` (Streamable HTTP, protocol 2025-06-18), reading
     `structuredContent`; the first three reads of every service are also called through
     `bee mcp` (stdio) and must match `/mcp`.

   The outcomes are compared after normalisation: on success the response messages (every
   item for streams; open event streams up to the fixture's `maxItems`), on failure the
   reason, `retryable`, subcode and message. A form passes when its outcome equals the
   fixture's `expect` (`ok` or a reason), equals the API's, and its own contract checks hold.

By default the `bee` program is this test binary re-executed with
`BEE_CONFORMANCE_AS_CLI=1`, whose `main` is the same line as `crates/bee-cli/src/main.rs`
(a dev-dependency cannot build a binary target, and a stale `target/debug/bee` would test old
code). `BEE_CONFORMANCE_BEE_BIN=target/debug/bee` runs the real binary instead.

## 2. Fresh state per form

The runner starts a `bee-server --dev` on SQLite in a temporary directory, runs the shared
scenario (`fixtures/scenario.json`) through the API, stops it, and keeps its data directory.
Each service's fixtures then run in a child process (servers do not return their threads when
they stop, so one process cannot start 120 of them) that starts three servers on copies of that
directory, one per form. All three begin byte-identical and receive the same setup steps in
the same order, so a mutating RPC acts on equal state in each form, and only values minted
during the call differ.

`parity::normalize` hides exactly those values before comparing: each server's URL, host and
token; RFC 3339 timestamps and Unix times in strings; and, by positional renaming (`<id:1>`,
`<id:2>`, ... in order of first appearance), ULIDs, hex runs of 32+ digits, `raw` id bytes,
long base64 strings, and scalars under volatile keys (`requestId`, tokens and secrets, URLs,
keys ending in `At`, `AtMs` or `Ms`). Positional names keep structure: one id used twice must
be the same id in every form. A fixture may list further JSON pointers in `volatile` for values
that legitimately differ between identical servers (a random suffix, a worker's progress).

## 3. Fixtures

`fixtures/scenario.json` seeds what most fixtures read: a changeset landed on `main` of
`dev/playground` (`{{base.*}}`) and an issue (`{{issue.*}}`). A fixture file is
`{"service": "bee.v1.<S>", "fixtures": [...]}`; each fixture has `method`, optional
`setup` steps, the `request` (strings are `{{var.path}}` templates over the built-ins `repo`,
`org`, `user`, `userName`, `server`, `token`, the scenario and setup results), `expect` (`ok` or
a SPEC §12.2 reason, which then needs a `note`), and optional `token`, `save`, `volatile` and
`maxItems`. The format is documented in `parity::fixture`. A setup step is an RPC (`call`) or
one of three built-ins that do what a client does outside the RPC surface:

- `commit`: upload files, create a changeset, then land it or create a ref;
- `seal`: seal a secret value for a scope's X25519 public key (libsodium sealed box), as
  `SecretService.PutSecret` expects;
- `upload`: send bytes to a signed URL a response handed out (package files, workflow
  artifacts and caches).

`token` on a fixture or step calls as another principal (an agent, a runner, a job token),
in every form; `save` keeps a fixture's response (each form's own) for later fixtures of the
same file. Rules the fixtures follow:

- Requests are complete and valid proto3-JSON (the runner rejects one that does not encode,
  so no form ever refuses a fixture locally). An error is pinned only when success is not
  possible against one dev server, with the reason in `note`.
- Mutating and destructive RPCs act on resources their own setup creates, with unique names;
  nothing deletes, renames or locks the shared `dev` org, `dev/playground` or the caller.
- Ids and numbers made by the server are always templated, never literal.

## 4. Running

```sh
cargo test -p bee-conformance --test three_forms                       # the gate run
BEE_CONFORMANCE_FULL=1 cargo test -p bee-conformance --test three_forms   # every service
```

| Variable | Effect |
|---|---|
| `BEE_CONFORMANCE_FULL=1` | Run every service (default: the services in `DEFAULT_SERVICES`). |
| `BEE_CONFORMANCE_SERVICES=Issue,Change` | Run only these services. |
| `BEE_CONFORMANCE_METHODS=CreateIssue` | Run only these fixtures. |
| `BEE_CONFORMANCE_FORMS=api` | Probe mode: API form only, print every outcome (for writing fixtures). |
| `BEE_CONFORMANCE_SHOW=1` | Print responses and requests in probe output. |
| `BEE_CONFORMANCE_VERBOSE=1` | Print every fixture's outcome and failures. |
| `BEE_CONFORMANCE_JOBS=4` | Services run at once (one child process each). |
| `BEE_CONFORMANCE_BEE_BIN=<path>` | Use this `bee` binary for the CLI form and stdio MCP. |
| `BEE_CONFORMANCE_REPORT=1` | With the full matrix, rewrite the table below. |

The table below is generated by the full run
(`BEE_CONFORMANCE_FULL=1 BEE_CONFORMANCE_REPORT=1`); the gate test
`verification_report_lists_every_served_rpc` fails when it no longer lists exactly the served
RPCs.

## 5. Findings

Disagreements and bugs the suite found, and what became of them:

- **`bee mcp` did not exist.** SPEC §12.4 and `bee-mcp` describe a stdio server started as
  `bee mcp`, but the CLI never wired it. Added (`crates/bee-cli/src/commands/mcp.rs`,
  tests in `crates/bee-cli/tests/mcp_stdio.rs`); it takes the CLI's endpoint and token and
  `--toolsets`.
- **The CLI and MCP changed numbers.** Without serde_json's `float_roundtrip` a standalone
  `bee` re-printed `13.274336283185841` as `13.27433628318584`; test builds hid it because a
  dev-dependency turns the feature on. Both manifests now ask for it
  (`crates/bee-cli/tests/json_numbers.rs`).
- **Native package versions could not be published.** `BeginPackageUpload` handed out
  `/pkg/...` upload URLs that no route served. `bee-server` now serves them, verifying the
  forge's signature (`native_package_uploads_go_to_the_signed_urls` in
  `crates/bee-server/tests/forge_packages.rs`).
- **Scoped discussion search found nothing.** `repo:`, `org:` and `user:` chose the repos but
  then failed the per-discussion filter, so `SearchDiscussions` returned no hits for any
  scoped query; fixed in `bee-server` (`discussion_search_honours_scope_qualifiers` in
  `crates/bee-server/tests/remaining_services.rs`).
- **A cancelled export finished anyway.** `ImportService` wrote RUNNING and per-repo progress
  over a CANCELLED record, so an export cancelled while queued or running still ended
  SUCCEEDED; now the job stops (`an_export_cancelled_while_queued_stays_cancelled` in
  `crates/bee-server/tests/remaining_services.rs`).
- **An audit sink update could be lost.** The sink delivery worker wrote back a row it had
  read before a concurrent `UpdateAuditSink`, reverting the update (seen as forms disagreeing
  on `ListAuditSinks`). It now compare-and-sets
  (`audit_sink_updates_survive_the_delivery_worker` in
  `crates/bee-server/tests/remaining_services.rs`).
- **Wrong reason for job-token RPCs.** `FinalizeWorkflowArtifact` and `CommitWorkflowCache`
  answered `INVALID_ARGUMENT` "`job_id` is required" to callers without a job token, although
  the requests have no `job_id`; now `PERMISSION_DENIED` (regression test in
  `crates/bee-forge/tests/ci.rs`).
- **RPCs that are served but unimplemented for valid requests.** The allow-list in
  `forge_services.rs` is now empty, and its test calls with `{}`, which these reject before
  reaching their `UNIMPLEMENTED`/`served_elsewhere` answer: `RepoService`
  SyncRepoMirror, CreateSocialPreviewUpload; `UserService` SSH keys and
  sessions (signing keys, contribution graph, activity overview, feed events and achievements
  are served now and have success fixtures);
  `ModerationService` MinimizeComment, UnminimizeComment (no comment source is wired) and
  GetModerationCommunityProfile; `SecurityService` RequestCodeScanningAutofix and
  VerifyObjectSignature (ports left as stubs); `DeploymentService`
  ListDeploymentProtectionRuleApps; `BillingService` plan, invoice, payment and email RPCs
  (`billing_disabled`). Their fixtures pin the answer, so all three forms are checked to agree.
- **No RPC reaches some forge state**, so their fixtures pin `NOT_FOUND`/`INVALID_STATE`:
  the model catalog (`Models::upsert_model`), prompt-file indexing
  (`Models::index_prompt_file`), creating an enterprise, release attestations, package
  provenance. (Agent definitions were in this list until `AgentService.RegisterAgent` landed;
  the agent run, watcher and anomaly fixtures now succeed.)
- Smaller server notes: `AuditLogFilter.repo` given by full name never matches (entries store
  the repo id); `AssignOrgRole.custom_role_id` and `CreateDeploymentProtectionRule.app_id`
  are not checked for existence; a principal without a role on a repo was seen to reach secret scanning
  on `Land` and creates a push-protection block before being refused.
- **Signed-URL grants differed between forms.** `ListExports` URLs carry `g=`, base64url JSON
  of the repos the URL was minted for; a fixture that creates its repo per server gets a
  different repo id in each form. The server binds the grant to the export's repos (correct),
  so the normaliser now decodes `g=` and renames the ids inside it (unit test
  `url_grants_are_read_through`).
- **Harness note.** A stopped `RunningServer` keeps its forge SQLite connection threads, so a
  process cannot start more than a few dozen servers; the runner uses one child process per
  service.

## 6. Results

<!-- BEGIN GENERATED: three-form conformance matrix -->

RPCs in the descriptor: 1141. Served: 1141; on the UNIMPLEMENTED allow-list: 0. Fixtures: 1141 (1091 expect success, 50 expect a documented error).

| Form | Passing | Of |
|---|---:|---:|
| API (HTTP/JSON; gRPC sampled) | 1141 | 1141 |
| CLI (`bee <noun> <verb> --json`) | 1141 | 1141 |
| MCP (`/mcp` tools/call; stdio sampled) | 1141 | 1141 |
| All three | 1141 | 1141 |

### By service

| Service | Methods | Expect ok | API | CLI | MCP |
|---|---:|---:|---:|---:|---:|
| AdminService | 13 | 13 | 13 | 13 | 13 |
| AgentService | 54 | 54 | 54 | 54 | 54 |
| AppService | 41 | 40 | 41 | 41 | 41 |
| AuditService | 12 | 12 | 12 | 12 | 12 |
| AuthService | 18 | 17 | 18 | 18 | 18 |
| BillingService | 22 | 16 | 22 | 22 | 22 |
| BriefService | 9 | 9 | 9 | 9 | 9 |
| ChangeService | 49 | 49 | 49 | 49 | 49 |
| CheckService | 19 | 19 | 19 | 19 | 19 |
| CommentService | 8 | 8 | 8 | 8 | 8 |
| DeploymentService | 20 | 19 | 20 | 20 | 20 |
| DiscussionService | 31 | 31 | 31 | 31 | 31 |
| EnterpriseService | 34 | 23 | 34 | 34 | 34 |
| EventService | 12 | 12 | 12 | 12 | 12 |
| GistService | 15 | 15 | 15 | 15 | 15 |
| GitService | 12 | 12 | 12 | 12 | 12 |
| ImportService | 12 | 10 | 12 | 12 | 12 |
| InsightsService | 15 | 15 | 15 | 15 | 15 |
| IssueService | 67 | 67 | 67 | 67 | 67 |
| LeaseService | 5 | 5 | 5 | 5 | 5 |
| ModelService | 14 | 7 | 14 | 14 | 14 |
| ModerationService | 30 | 27 | 30 | 30 | 30 |
| NotificationService | 26 | 26 | 26 | 26 | 26 |
| ObjectService | 6 | 6 | 6 | 6 | 6 |
| OpsService | 25 | 22 | 25 | 25 | 25 |
| OrgService | 20 | 20 | 20 | 20 | 20 |
| PackageService | 24 | 23 | 24 | 24 | 24 |
| PagesService | 19 | 17 | 19 | 19 | 19 |
| PolicyService | 25 | 25 | 25 | 25 | 25 |
| PostService | 16 | 16 | 16 | 16 | 16 |
| ProjectService | 50 | 50 | 50 | 50 | 50 |
| QueryService | 13 | 13 | 13 | 13 | 13 |
| RefService | 6 | 6 | 6 | 6 | 6 |
| ReleaseService | 23 | 22 | 23 | 23 | 23 |
| RepoService | 43 | 43 | 43 | 43 | 43 |
| RunnerService | 28 | 28 | 28 | 28 | 28 |
| SearchService | 26 | 25 | 26 | 26 | 26 |
| SecretService | 17 | 17 | 17 | 17 | 17 |
| SecurityService | 63 | 62 | 63 | 63 | 63 |
| SponsorService | 14 | 13 | 14 | 14 | 14 |
| TeamService | 22 | 22 | 22 | 22 | 22 |
| UserService | 58 | 51 | 58 | 58 | 58 |
| WikiService | 14 | 14 | 14 | 14 | 14 |
| WorkflowService | 60 | 60 | 60 | 60 | 60 |
| WorkspaceService | 25 | 24 | 25 | 25 | 25 |
| WriteService | 6 | 6 | 6 | 6 | 6 |

### By method

| Service | Method | Expect | API | CLI | MCP |
|---|---|---|---|---|---|
| AdminService | CreateBackup | ok | pass | pass | pass |
| AdminService | GetRepoHealth | ok | pass | pass | pass |
| AdminService | GetRestore | ok | pass | pass | pass |
| AdminService | ListBackups | ok | pass | pass | pass |
| AdminService | ListRedactions | ok | pass | pass | pass |
| AdminService | PurgeNow | ok | pass | pass | pass |
| AdminService | Redact | ok | pass | pass | pass |
| AdminService | RetireSigningKey | ok | pass | pass | pass |
| AdminService | RotateSigningKey | ok | pass | pass | pass |
| AdminService | RunFsck | ok | pass | pass | pass |
| AdminService | RunGc | ok | pass | pass | pass |
| AdminService | StartRestore | ok | pass | pass | pass |
| AdminService | Unredact | ok | pass | pass | pass |
| AgentService | AcknowledgeWatcherAnomaly | ok | pass | pass | pass |
| AgentService | CancelAgentRun | ok | pass | pass | pass |
| AgentService | CreateComponentWatcher | ok | pass | pass | pass |
| AgentService | CreateContextBundle | ok | pass | pass | pass |
| AgentService | DeleteAgent | ok | pass | pass | pass |
| AgentService | DeleteAgentMemory | ok | pass | pass | pass |
| AgentService | DeleteAgentRuntime | ok | pass | pass | pass |
| AgentService | DeleteComponentWatcher | ok | pass | pass | pass |
| AgentService | DeleteContextBundle | ok | pass | pass | pass |
| AgentService | GetAgent | ok | pass | pass | pass |
| AgentService | GetAgentBudget | ok | pass | pass | pass |
| AgentService | GetAgentDashboard | ok | pass | pass | pass |
| AgentService | GetAgentFeedTrigger | ok | pass | pass | pass |
| AgentService | GetAgentMemory | ok | pass | pass | pass |
| AgentService | GetAgentReviewConfig | ok | pass | pass | pass |
| AgentService | GetAgentRun | ok | pass | pass | pass |
| AgentService | GetAgentRuntime | ok | pass | pass | pass |
| AgentService | GetComponentWatcher | ok | pass | pass | pass |
| AgentService | GetContextBundle | ok | pass | pass | pass |
| AgentService | GetSessionsForChange | ok | pass | pass | pass |
| AgentService | GetWatcherAnomaly | ok | pass | pass | pass |
| AgentService | LinkWatcherAnomalyToIssue | ok | pass | pass | pass |
| AgentService | ListAgentActivity | ok | pass | pass | pass |
| AgentService | ListAgentFeedTriggers | ok | pass | pass | pass |
| AgentService | ListAgentInstructionFiles | ok | pass | pass | pass |
| AgentService | ListAgentMemories | ok | pass | pass | pass |
| AgentService | ListAgentPrincipals | ok | pass | pass | pass |
| AgentService | ListAgentRunEvents | ok | pass | pass | pass |
| AgentService | ListAgentRuns | ok | pass | pass | pass |
| AgentService | ListAgentRuntimes | ok | pass | pass | pass |
| AgentService | ListAgentWorkItems | ok | pass | pass | pass |
| AgentService | ListAgents | ok | pass | pass | pass |
| AgentService | ListComponentWatchers | ok | pass | pass | pass |
| AgentService | ListContextBundles | ok | pass | pass | pass |
| AgentService | ListWatcherAnomalies | ok | pass | pass | pass |
| AgentService | MaterializeContextBundle | ok | pass | pass | pass |
| AgentService | PutAgentMemory | ok | pass | pass | pass |
| AgentService | RegisterAgent | ok | pass | pass | pass |
| AgentService | RegisterAgentRuntime | ok | pass | pass | pass |
| AgentService | ReportWatcherAnomaly | ok | pass | pass | pass |
| AgentService | ResolveAgentInstructions | ok | pass | pass | pass |
| AgentService | RetryAgentRun | ok | pass | pass | pass |
| AgentService | SearchAgentMemories | ok | pass | pass | pass |
| AgentService | SendAgentRunMessage | ok | pass | pass | pass |
| AgentService | SetAgentBudget | ok | pass | pass | pass |
| AgentService | SetAgentFeedTrigger | ok | pass | pass | pass |
| AgentService | SetAgentReviewConfig | ok | pass | pass | pass |
| AgentService | StartAgentRun | ok | pass | pass | pass |
| AgentService | StreamAgentRunEvents | ok | pass | pass | pass |
| AgentService | StreamWatcherAnomalies | ok | pass | pass | pass |
| AgentService | UpdateAgent | ok | pass | pass | pass |
| AgentService | UpdateAgentRuntime | ok | pass | pass | pass |
| AgentService | UpdateComponentWatcher | ok | pass | pass | pass |
| AgentService | UpdateContextBundle | ok | pass | pass | pass |
| AppService | AddAppInstallationRepos | ok | pass | pass | pass |
| AppService | CancelMarketplacePurchase | ok | pass | pass | pass |
| AppService | CreateAppFromManifest | ok | pass | pass | pass |
| AppService | CreateAppInstallation | ok | pass | pass | pass |
| AppService | CreateAppPrivateKey | ok | pass | pass | pass |
| AppService | CreateInstallationToken | ok | pass | pass | pass |
| AppService | CreateMarketplaceListing | ok | pass | pass | pass |
| AppService | CreateMarketplacePlan | ok | pass | pass | pass |
| AppService | CreateOAuthApp | ok | pass | pass | pass |
| AppService | CreateOAuthAuthorization | ok | pass | pass | pass |
| AppService | DeleteApp | ok | pass | pass | pass |
| AppService | DeleteAppInstallation | ok | pass | pass | pass |
| AppService | DeleteAppPrivateKey | ok | pass | pass | pass |
| AppService | DeleteOAuthApp | ok | pass | pass | pass |
| AppService | ExchangeOAuthCode | ok | pass | pass | pass |
| AppService | GetApp | ok | pass | pass | pass |
| AppService | GetAppInstallation | ok | pass | pass | pass |
| AppService | GetMarketplaceListing | ok | pass | pass | pass |
| AppService | GetMarketplacePublisher | ok | pass | pass | pass |
| AppService | GetOAuthApp | ok | pass | pass | pass |
| AppService | ListAppInstallationRepos | ok | pass | pass | pass |
| AppService | ListAppInstallations | ok | pass | pass | pass |
| AppService | ListApps | ok | pass | pass | pass |
| AppService | ListMarketplaceListings | ok | pass | pass | pass |
| AppService | ListMarketplacePurchases | ok | pass | pass | pass |
| AppService | ListOAuthApps | ok | pass | pass | pass |
| AppService | ListOAuthAuthorizedApps | ok | pass | pass | pass |
| AppService | PollOAuthDeviceFlow | ok | pass | pass | pass |
| AppService | PublishMarketplaceListing | ok | pass | pass | pass |
| AppService | PurchaseMarketplacePlan | ok | pass | pass | pass |
| AppService | RefreshOAuthToken | ok | pass | pass | pass |
| AppService | RemoveAppInstallationRepos | ok | pass | pass | pass |
| AppService | RequestMarketplacePublisherVerification | `INVALID_STATE` | pass | pass | pass |
| AppService | RevokeOAuthGrant | ok | pass | pass | pass |
| AppService | StartOAuthDeviceFlow | ok | pass | pass | pass |
| AppService | SuspendAppInstallation | ok | pass | pass | pass |
| AppService | UnsuspendAppInstallation | ok | pass | pass | pass |
| AppService | UpdateApp | ok | pass | pass | pass |
| AppService | UpdateMarketplaceListing | ok | pass | pass | pass |
| AppService | UpdateMarketplacePlan | ok | pass | pass | pass |
| AppService | UpdateOAuthApp | ok | pass | pass | pass |
| AuditService | CreateAuditSink | ok | pass | pass | pass |
| AuditService | DeleteAuditSink | ok | pass | pass | pass |
| AuditService | ExportAuditLog | ok | pass | pass | pass |
| AuditService | GetAuditEntry | ok | pass | pass | pass |
| AuditService | GetAuditExport | ok | pass | pass | pass |
| AuditService | GetAuditSink | ok | pass | pass | pass |
| AuditService | ListAuditSinks | ok | pass | pass | pass |
| AuditService | QueryAuditLog | ok | pass | pass | pass |
| AuditService | StreamAuditLog | ok | pass | pass | pass |
| AuditService | TestAuditSink | ok | pass | pass | pass |
| AuditService | UpdateAuditSink | ok | pass | pass | pass |
| AuditService | VerifyAuditChain | ok | pass | pass | pass |
| AuthService | BeginSecondFactorEnrollment | ok | pass | pass | pass |
| AuthService | CheckAccess | ok | pass | pass | pass |
| AuthService | CheckObjectAccess | ok | pass | pass | pass |
| AuthService | CreateAgent | ok | pass | pass | pass |
| AuthService | DeactivatePrincipal | ok | pass | pass | pass |
| AuthService | DeleteSecondFactor | ok | pass | pass | pass |
| AuthService | FinishSecondFactorEnrollment | ok | pass | pass | pass |
| AuthService | GetPrincipal | ok | pass | pass | pass |
| AuthService | ListSecondFactors | ok | pass | pass | pass |
| AuthService | ListSigningKeys | ok | pass | pass | pass |
| AuthService | ListTokens | ok | pass | pass | pass |
| AuthService | Login | ok | pass | pass | pass |
| AuthService | Logout | ok | pass | pass | pass |
| AuthService | MintToken | ok | pass | pass | pass |
| AuthService | Revoke | ok | pass | pass | pass |
| AuthService | UpdateAgentPrincipal | ok | pass | pass | pass |
| AuthService | VerifySecondFactor | `UNAUTHENTICATED` | pass | pass | pass |
| AuthService | WhoAmI | ok | pass | pass | pass |
| BillingService | AddCostCenterResources | ok | pass | pass | pass |
| BillingService | ChangeBillingPlan | `UNIMPLEMENTED` | pass | pass | pass |
| BillingService | CreateBillingBudget | ok | pass | pass | pass |
| BillingService | CreateCostCenter | ok | pass | pass | pass |
| BillingService | DeleteBillingBudget | ok | pass | pass | pass |
| BillingService | DeleteCostCenter | ok | pass | pass | pass |
| BillingService | GetAccountBillingPlan | ok | pass | pass | pass |
| BillingService | GetBillingBudget | ok | pass | pass | pass |
| BillingService | GetBillingEmailSettings | `UNIMPLEMENTED` | pass | pass | pass |
| BillingService | GetBillingPaymentMethod | `UNIMPLEMENTED` | pass | pass | pass |
| BillingService | GetCostCenter | ok | pass | pass | pass |
| BillingService | GetInvoice | `UNIMPLEMENTED` | pass | pass | pass |
| BillingService | ListBillingBudgets | ok | pass | pass | pass |
| BillingService | ListBillingPlans | ok | pass | pass | pass |
| BillingService | ListBillingSeats | ok | pass | pass | pass |
| BillingService | ListCostCenters | ok | pass | pass | pass |
| BillingService | ListInvoices | `UNIMPLEMENTED` | pass | pass | pass |
| BillingService | QueryBillingUsage | ok | pass | pass | pass |
| BillingService | RemoveCostCenterResources | ok | pass | pass | pass |
| BillingService | UpdateBillingBudget | ok | pass | pass | pass |
| BillingService | UpdateBillingEmailSettings | `UNIMPLEMENTED` | pass | pass | pass |
| BillingService | UpdateCostCenter | ok | pass | pass | pass |
| BriefService | AddEvidence | ok | pass | pass | pass |
| BriefService | BeginEvidenceUpload | ok | pass | pass | pass |
| BriefService | DeleteEvidence | ok | pass | pass | pass |
| BriefService | FinalizeEvidence | ok | pass | pass | pass |
| BriefService | GetBriefChecklist | ok | pass | pass | pass |
| BriefService | GetEvidence | ok | pass | pass | pass |
| BriefService | ListEvidence | ok | pass | pass | pass |
| BriefService | SetChangeBrief | ok | pass | pass | pass |
| BriefService | UpdateEvidence | ok | pass | pass | pass |
| ChangeService | AddChangeReaction | ok | pass | pass | pass |
| ChangeService | ApplySuggestions | ok | pass | pass | pass |
| ChangeService | CloseChange | ok | pass | pass | pass |
| ChangeService | ConvertChangeToDraft | ok | pass | pass | pass |
| ChangeService | CreateChangeComment | ok | pass | pass | pass |
| ChangeService | CreateChangesetComment | ok | pass | pass | pass |
| ChangeService | DequeueChange | ok | pass | pass | pass |
| ChangeService | DiffChangeRevisions | ok | pass | pass | pass |
| ChangeService | DisableAutoMerge | ok | pass | pass | pass |
| ChangeService | DismissReview | ok | pass | pass | pass |
| ChangeService | EditChange | ok | pass | pass | pass |
| ChangeService | EnableAutoMerge | ok | pass | pass | pass |
| ChangeService | EnqueueChange | ok | pass | pass | pass |
| ChangeService | EvaluateLanding | ok | pass | pass | pass |
| ChangeService | GetChange | ok | pass | pass | pass |
| ChangeService | GetChangeByNumber | ok | pass | pass | pass |
| ChangeService | GetLandConflict | ok | pass | pass | pass |
| ChangeService | GetMergeQueue | ok | pass | pass | pass |
| ChangeService | LandChange | ok | pass | pass | pass |
| ChangeService | LinkChangeIssues | ok | pass | pass | pass |
| ChangeService | ListChangeComments | ok | pass | pass | pass |
| ChangeService | ListChangeStack | ok | pass | pass | pass |
| ChangeService | ListChangeTemplates | ok | pass | pass | pass |
| ChangeService | ListChanges | ok | pass | pass | pass |
| ChangeService | ListChangesetComments | ok | pass | pass | pass |
| ChangeService | ListLandConflicts | ok | pass | pass | pass |
| ChangeService | ListReviewThreads | ok | pass | pass | pass |
| ChangeService | ListReviews | ok | pass | pass | pass |
| ChangeService | ListViewedFiles | ok | pass | pass | pass |
| ChangeService | MarkChangeReady | ok | pass | pass | pass |
| ChangeService | OpenChange | ok | pass | pass | pass |
| ChangeService | RemoveChangeReaction | ok | pass | pass | pass |
| ChangeService | RemoveReviewRequests | ok | pass | pass | pass |
| ChangeService | ReopenChange | ok | pass | pass | pass |
| ChangeService | ReplyToReviewThread | ok | pass | pass | pass |
| ChangeService | RequestReviewers | ok | pass | pass | pass |
| ChangeService | ResolveReviewThread | ok | pass | pass | pass |
| ChangeService | RevertChange | ok | pass | pass | pass |
| ChangeService | ReviseChange | ok | pass | pass | pass |
| ChangeService | SetChangeAssignees | ok | pass | pass | pass |
| ChangeService | SetChangeLabels | ok | pass | pass | pass |
| ChangeService | SetChangeLocked | ok | pass | pass | pass |
| ChangeService | SetChangeMilestone | ok | pass | pass | pass |
| ChangeService | SetChangeSummary | ok | pass | pass | pass |
| ChangeService | SetFileViewed | ok | pass | pass | pass |
| ChangeService | SubmitConflictResolution | ok | pass | pass | pass |
| ChangeService | SubmitReview | ok | pass | pass | pass |
| ChangeService | UnresolveReviewThread | ok | pass | pass | pass |
| ChangeService | UpdateChangeBase | ok | pass | pass | pass |
| CheckService | CancelBisect | ok | pass | pass | pass |
| CheckService | CreateCheckSuite | ok | pass | pass | pass |
| CheckService | CreateCommitStatus | ok | pass | pass | pass |
| CheckService | GetBisect | ok | pass | pass | pass |
| CheckService | GetBisectNextCandidate | ok | pass | pass | pass |
| CheckService | GetBisectSession | ok | pass | pass | pass |
| CheckService | GetCheckRun | ok | pass | pass | pass |
| CheckService | GetCombinedStatus | ok | pass | pass | pass |
| CheckService | ListBisects | ok | pass | pass | pass |
| CheckService | ListCheckAnnotations | ok | pass | pass | pass |
| CheckService | ListCheckSuites | ok | pass | pass | pass |
| CheckService | ListChecks | ok | pass | pass | pass |
| CheckService | ListCommitStatuses | ok | pass | pass | pass |
| CheckService | MarkBisect | ok | pass | pass | pass |
| CheckService | ReportCheck | ok | pass | pass | pass |
| CheckService | RerequestCheckRun | ok | pass | pass | pass |
| CheckService | RerequestCheckSuite | ok | pass | pass | pass |
| CheckService | StartBisect | ok | pass | pass | pass |
| CheckService | UpdateCheckRun | ok | pass | pass | pass |
| CommentService | AddReaction | ok | pass | pass | pass |
| CommentService | DeleteComment | ok | pass | pass | pass |
| CommentService | GetComment | ok | pass | pass | pass |
| CommentService | ListCommentEdits | ok | pass | pass | pass |
| CommentService | ListReactions | ok | pass | pass | pass |
| CommentService | MarkSubjectRead | ok | pass | pass | pass |
| CommentService | RemoveReaction | ok | pass | pass | pass |
| CommentService | UpdateComment | ok | pass | pass | pass |
| DeploymentService | CreateDeployment | ok | pass | pass | pass |
| DeploymentService | CreateDeploymentEnvironment | ok | pass | pass | pass |
| DeploymentService | CreateDeploymentProtectionRule | ok | pass | pass | pass |
| DeploymentService | CreateDeploymentStatus | ok | pass | pass | pass |
| DeploymentService | DeleteDeployment | ok | pass | pass | pass |
| DeploymentService | DeleteDeploymentEnvironment | ok | pass | pass | pass |
| DeploymentService | DeleteDeploymentProtectionRule | ok | pass | pass | pass |
| DeploymentService | GetDeployment | ok | pass | pass | pass |
| DeploymentService | GetDeploymentEnvironment | ok | pass | pass | pass |
| DeploymentService | GetDeploymentGreenStatus | ok | pass | pass | pass |
| DeploymentService | GetDeploymentStatus | ok | pass | pass | pass |
| DeploymentService | ListDeploymentEnvironments | ok | pass | pass | pass |
| DeploymentService | ListDeploymentProtectionRuleApps | `UNIMPLEMENTED` | pass | pass | pass |
| DeploymentService | ListDeploymentReviews | ok | pass | pass | pass |
| DeploymentService | ListDeploymentStatuses | ok | pass | pass | pass |
| DeploymentService | ListDeployments | ok | pass | pass | pass |
| DeploymentService | ListPendingDeploymentReviews | ok | pass | pass | pass |
| DeploymentService | ReportDeploymentProtectionRuleResult | ok | pass | pass | pass |
| DeploymentService | ReviewPendingDeployment | ok | pass | pass | pass |
| DeploymentService | UpdateDeploymentEnvironment | ok | pass | pass | pass |
| DiscussionService | AddDiscussionLabels | ok | pass | pass | pass |
| DiscussionService | AddDiscussionUpvote | ok | pass | pass | pass |
| DiscussionService | CloseDiscussion | ok | pass | pass | pass |
| DiscussionService | ConvertDiscussionToIssue | ok | pass | pass | pass |
| DiscussionService | ConvertIssueToDiscussion | ok | pass | pass | pass |
| DiscussionService | CreateDiscussion | ok | pass | pass | pass |
| DiscussionService | CreateDiscussionCategory | ok | pass | pass | pass |
| DiscussionService | CreateDiscussionComment | ok | pass | pass | pass |
| DiscussionService | DeleteDiscussion | ok | pass | pass | pass |
| DiscussionService | DeleteDiscussionCategory | ok | pass | pass | pass |
| DiscussionService | GetDiscussion | ok | pass | pass | pass |
| DiscussionService | GetDiscussionByNumber | ok | pass | pass | pass |
| DiscussionService | GetDiscussionPoll | ok | pass | pass | pass |
| DiscussionService | ListDiscussionCategories | ok | pass | pass | pass |
| DiscussionService | ListDiscussionCommentReplies | ok | pass | pass | pass |
| DiscussionService | ListDiscussionComments | ok | pass | pass | pass |
| DiscussionService | ListDiscussions | ok | pass | pass | pass |
| DiscussionService | ListPinnedDiscussions | ok | pass | pass | pass |
| DiscussionService | LockDiscussion | ok | pass | pass | pass |
| DiscussionService | MarkDiscussionAnswer | ok | pass | pass | pass |
| DiscussionService | PinDiscussion | ok | pass | pass | pass |
| DiscussionService | RemoveDiscussionLabels | ok | pass | pass | pass |
| DiscussionService | RemoveDiscussionUpvote | ok | pass | pass | pass |
| DiscussionService | ReopenDiscussion | ok | pass | pass | pass |
| DiscussionService | SetDiscussionLabels | ok | pass | pass | pass |
| DiscussionService | UnlockDiscussion | ok | pass | pass | pass |
| DiscussionService | UnmarkDiscussionAnswer | ok | pass | pass | pass |
| DiscussionService | UnpinDiscussion | ok | pass | pass | pass |
| DiscussionService | UpdateDiscussion | ok | pass | pass | pass |
| DiscussionService | UpdateDiscussionCategory | ok | pass | pass | pass |
| DiscussionService | VoteDiscussionPoll | ok | pass | pass | pass |
| EnterpriseService | AddEnterpriseOrg | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | AddEnterpriseOwner | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | AddVerifiedDomain | ok | pass | pass | pass |
| EnterpriseService | CheckVerifiedDomain | ok | pass | pass | pass |
| EnterpriseService | CreateIpAllowListEntry | ok | pass | pass | pass |
| EnterpriseService | CreateScimToken | ok | pass | pass | pass |
| EnterpriseService | DeleteIpAllowListEntry | ok | pass | pass | pass |
| EnterpriseService | DeleteVerifiedDomain | ok | pass | pass | pass |
| EnterpriseService | GetEnterprise | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | GetEnterprisePolicies | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | GetIpAllowListSettings | ok | pass | pass | pass |
| EnterpriseService | GetSamlSsoConfig | ok | pass | pass | pass |
| EnterpriseService | GetScimConfig | ok | pass | pass | pass |
| EnterpriseService | GetTwoFactorRequirement | ok | pass | pass | pass |
| EnterpriseService | GetVerifiedDomainSettings | ok | pass | pass | pass |
| EnterpriseService | ListEnterpriseOrgs | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | ListEnterpriseOwners | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | ListIpAllowListEntries | ok | pass | pass | pass |
| EnterpriseService | ListSamlIdentities | ok | pass | pass | pass |
| EnterpriseService | ListScimIdentities | ok | pass | pass | pass |
| EnterpriseService | ListTwoFactorNoncompliantMembers | ok | pass | pass | pass |
| EnterpriseService | ListVerifiedDomains | ok | pass | pass | pass |
| EnterpriseService | RemoveEnterpriseOrg | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | RemoveEnterpriseOwner | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | RevokeSamlSession | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | TestSamlSsoConfig | ok | pass | pass | pass |
| EnterpriseService | UpdateEnterprise | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | UpdateEnterprisePolicies | `NOT_FOUND` | pass | pass | pass |
| EnterpriseService | UpdateIpAllowListEntry | ok | pass | pass | pass |
| EnterpriseService | UpdateIpAllowListSettings | ok | pass | pass | pass |
| EnterpriseService | UpdateSamlSsoConfig | ok | pass | pass | pass |
| EnterpriseService | UpdateScimConfig | ok | pass | pass | pass |
| EnterpriseService | UpdateTwoFactorRequirement | ok | pass | pass | pass |
| EnterpriseService | UpdateVerifiedDomainSettings | ok | pass | pass | pass |
| EventService | CreateWebhook | ok | pass | pass | pass |
| EventService | DeleteWebhook | ok | pass | pass | pass |
| EventService | GetEventLogInfo | ok | pass | pass | pass |
| EventService | GetWebhook | ok | pass | pass | pass |
| EventService | GetWebhookDelivery | ok | pass | pass | pass |
| EventService | ListEvents | ok | pass | pass | pass |
| EventService | ListWebhookDeliveries | ok | pass | pass | pass |
| EventService | ListWebhooks | ok | pass | pass | pass |
| EventService | PingWebhook | ok | pass | pass | pass |
| EventService | RedeliverWebhook | ok | pass | pass | pass |
| EventService | Subscribe | ok | pass | pass | pass |
| EventService | UpdateWebhook | ok | pass | pass | pass |
| GistService | CreateGist | ok | pass | pass | pass |
| GistService | CreateGistComment | ok | pass | pass | pass |
| GistService | DeleteGist | ok | pass | pass | pass |
| GistService | ForkGist | ok | pass | pass | pass |
| GistService | GetGist | ok | pass | pass | pass |
| GistService | GetGistFileRawUrl | ok | pass | pass | pass |
| GistService | GetGistRevision | ok | pass | pass | pass |
| GistService | IsGistStarred | ok | pass | pass | pass |
| GistService | ListGistComments | ok | pass | pass | pass |
| GistService | ListGistForks | ok | pass | pass | pass |
| GistService | ListGistRevisions | ok | pass | pass | pass |
| GistService | ListGists | ok | pass | pass | pass |
| GistService | StarGist | ok | pass | pass | pass |
| GistService | UnstarGist | ok | pass | pass | pass |
| GistService | UpdateGist | ok | pass | pass | pass |
| GitService | CreateGitCredential | ok | pass | pass | pass |
| GitService | CreateGitPackUpload | ok | pass | pass | pass |
| GitService | GetGitConfig | ok | pass | pass | pass |
| GitService | GetGitObjects | ok | pass | pass | pass |
| GitService | GitIngestCommits | ok | pass | pass | pass |
| GitService | GitLfsBatch | ok | pass | pass | pass |
| GitService | GitListRefs | ok | pass | pass | pass |
| GitService | GitPlanFetch | ok | pass | pass | pass |
| GitService | GitPreflight | ok | pass | pass | pass |
| GitService | GitResolve | ok | pass | pass | pass |
| GitService | ResolveGitSshKey | ok | pass | pass | pass |
| GitService | UpdateGitConfig | ok | pass | pass | pass |
| ImportService | CancelExport | ok | pass | pass | pass |
| ImportService | CancelImport | ok | pass | pass | pass |
| ImportService | ClaimIdentity | `INVALID_STATE` | pass | pass | pass |
| ImportService | GetExport | ok | pass | pass | pass |
| ImportService | GetFidelityReport | `INVALID_STATE` | pass | pass | pass |
| ImportService | GetImport | ok | pass | pass | pass |
| ImportService | ListExports | ok | pass | pass | pass |
| ImportService | ListImports | ok | pass | pass | pass |
| ImportService | ListPlaceholderPrincipals | ok | pass | pass | pass |
| ImportService | SetImportPhase | ok | pass | pass | pass |
| ImportService | StartExport | ok | pass | pass | pass |
| ImportService | StartImport | ok | pass | pass | pass |
| InsightsService | GetCitation | ok | pass | pass | pass |
| InsightsService | GetCodeFrequency | ok | pass | pass | pass |
| InsightsService | GetCommitActivity | ok | pass | pass | pass |
| InsightsService | GetCommunityProfile | ok | pass | pass | pass |
| InsightsService | GetLanguageStats | ok | pass | pass | pass |
| InsightsService | GetLicenseInfo | ok | pass | pass | pass |
| InsightsService | GetNetworkGraph | ok | pass | pass | pass |
| InsightsService | GetPulse | ok | pass | pass | pass |
| InsightsService | GetTrafficClones | ok | pass | pass | pass |
| InsightsService | GetTrafficViews | ok | pass | pass | pass |
| InsightsService | ListContributors | ok | pass | pass | pass |
| InsightsService | ListInsightsAgentStats | ok | pass | pass | pass |
| InsightsService | ListInsightsOrgDependencies | ok | pass | pass | pass |
| InsightsService | ListTrafficPopularPaths | ok | pass | pass | pass |
| InsightsService | ListTrafficReferrers | ok | pass | pass | pass |
| IssueService | AddIssueAssignees | ok | pass | pass | pass |
| IssueService | AddIssueDependency | ok | pass | pass | pass |
| IssueService | AddIssueLabels | ok | pass | pass | pass |
| IssueService | AddIssueSubIssue | ok | pass | pass | pass |
| IssueService | ClearIssueFieldValue | ok | pass | pass | pass |
| IssueService | CloseIssue | ok | pass | pass | pass |
| IssueService | CreateIssue | ok | pass | pass | pass |
| IssueService | CreateIssueBranch | ok | pass | pass | pass |
| IssueService | CreateIssueComment | ok | pass | pass | pass |
| IssueService | CreateIssueField | ok | pass | pass | pass |
| IssueService | CreateIssueLabel | ok | pass | pass | pass |
| IssueService | CreateIssueMilestone | ok | pass | pass | pass |
| IssueService | CreateIssueType | ok | pass | pass | pass |
| IssueService | CreateSavedReply | ok | pass | pass | pass |
| IssueService | DeleteIssue | ok | pass | pass | pass |
| IssueService | DeleteIssueField | ok | pass | pass | pass |
| IssueService | DeleteIssueLabel | ok | pass | pass | pass |
| IssueService | DeleteIssueMilestone | ok | pass | pass | pass |
| IssueService | DeleteIssueType | ok | pass | pass | pass |
| IssueService | DeleteSavedReply | ok | pass | pass | pass |
| IssueService | GetIssue | ok | pass | pass | pass |
| IssueService | GetIssueByNumber | ok | pass | pass | pass |
| IssueService | GetIssueDevelopment | ok | pass | pass | pass |
| IssueService | GetIssueLabel | ok | pass | pass | pass |
| IssueService | GetIssueMilestone | ok | pass | pass | pass |
| IssueService | GetIssueParent | ok | pass | pass | pass |
| IssueService | GetIssueTaskListProgress | ok | pass | pass | pass |
| IssueService | GetIssueTemplateConfig | ok | pass | pass | pass |
| IssueService | GetSavedReply | ok | pass | pass | pass |
| IssueService | LinkIssueChange | ok | pass | pass | pass |
| IssueService | ListIssueAssignableUsers | ok | pass | pass | pass |
| IssueService | ListIssueComments | ok | pass | pass | pass |
| IssueService | ListIssueDependencies | ok | pass | pass | pass |
| IssueService | ListIssueFields | ok | pass | pass | pass |
| IssueService | ListIssueLabels | ok | pass | pass | pass |
| IssueService | ListIssueMilestones | ok | pass | pass | pass |
| IssueService | ListIssueSubIssues | ok | pass | pass | pass |
| IssueService | ListIssueTemplates | ok | pass | pass | pass |
| IssueService | ListIssueTimeline | ok | pass | pass | pass |
| IssueService | ListIssueTypes | ok | pass | pass | pass |
| IssueService | ListIssues | ok | pass | pass | pass |
| IssueService | ListPinnedIssues | ok | pass | pass | pass |
| IssueService | ListSavedReplies | ok | pass | pass | pass |
| IssueService | LockIssue | ok | pass | pass | pass |
| IssueService | MarkIssueDuplicate | ok | pass | pass | pass |
| IssueService | PinIssue | ok | pass | pass | pass |
| IssueService | RemoveIssueAssignees | ok | pass | pass | pass |
| IssueService | RemoveIssueDependency | ok | pass | pass | pass |
| IssueService | RemoveIssueLabels | ok | pass | pass | pass |
| IssueService | RemoveIssueSubIssue | ok | pass | pass | pass |
| IssueService | ReopenIssue | ok | pass | pass | pass |
| IssueService | ReorderIssueSubIssue | ok | pass | pass | pass |
| IssueService | SetIssueFieldValues | ok | pass | pass | pass |
| IssueService | SetIssueLabels | ok | pass | pass | pass |
| IssueService | SetIssueMilestone | ok | pass | pass | pass |
| IssueService | SetIssueType | ok | pass | pass | pass |
| IssueService | TransferIssue | ok | pass | pass | pass |
| IssueService | UnlinkIssueChange | ok | pass | pass | pass |
| IssueService | UnlockIssue | ok | pass | pass | pass |
| IssueService | UnmarkIssueDuplicate | ok | pass | pass | pass |
| IssueService | UnpinIssue | ok | pass | pass | pass |
| IssueService | UpdateIssue | ok | pass | pass | pass |
| IssueService | UpdateIssueField | ok | pass | pass | pass |
| IssueService | UpdateIssueLabel | ok | pass | pass | pass |
| IssueService | UpdateIssueMilestone | ok | pass | pass | pass |
| IssueService | UpdateIssueType | ok | pass | pass | pass |
| IssueService | UpdateSavedReply | ok | pass | pass | pass |
| LeaseService | AcquireLease | ok | pass | pass | pass |
| LeaseService | GetLease | ok | pass | pass | pass |
| LeaseService | ListLeases | ok | pass | pass | pass |
| LeaseService | ReleaseLease | ok | pass | pass | pass |
| LeaseService | RenewLease | ok | pass | pass | pass |
| ModelService | CancelEvalRun | `NOT_FOUND` | pass | pass | pass |
| ModelService | CreateModelProviderKey | ok | pass | pass | pass |
| ModelService | DeleteModelProviderKey | ok | pass | pass | pass |
| ModelService | GetEvalRun | `NOT_FOUND` | pass | pass | pass |
| ModelService | GetModel | `NOT_FOUND` | pass | pass | pass |
| ModelService | GetModelUsage | ok | pass | pass | pass |
| ModelService | GetPromptFile | `NOT_FOUND` | pass | pass | pass |
| ModelService | ListEvalRuns | ok | pass | pass | pass |
| ModelService | ListModelProviderKeys | ok | pass | pass | pass |
| ModelService | ListModels | ok | pass | pass | pass |
| ModelService | ListPromptFiles | ok | pass | pass | pass |
| ModelService | RunModelInference | `NOT_FOUND` | pass | pass | pass |
| ModelService | RunPromptEval | `NOT_FOUND` | pass | pass | pass |
| ModelService | StreamModelInference | `NOT_FOUND` | pass | pass | pass |
| ModerationService | AddModerator | ok | pass | pass | pass |
| ModerationService | BlockUser | ok | pass | pass | pass |
| ModerationService | CheckUserBlocked | ok | pass | pass | pass |
| ModerationService | CreateTakedown | ok | pass | pass | pass |
| ModerationService | GetContentReport | ok | pass | pass | pass |
| ModerationService | GetFirstTimeContributorSettings | ok | pass | pass | pass |
| ModerationService | GetFloodLimits | ok | pass | pass | pass |
| ModerationService | GetInteractionLimit | ok | pass | pass | pass |
| ModerationService | GetModerationCommunityProfile | `UNIMPLEMENTED` | pass | pass | pass |
| ModerationService | GetTakedown | ok | pass | pass | pass |
| ModerationService | ListBlockedUsers | ok | pass | pass | pass |
| ModerationService | ListCodesOfConduct | ok | pass | pass | pass |
| ModerationService | ListContentReports | ok | pass | pass | pass |
| ModerationService | ListModerators | ok | pass | pass | pass |
| ModerationService | ListTakedowns | ok | pass | pass | pass |
| ModerationService | ListThrottledPrincipals | ok | pass | pass | pass |
| ModerationService | LockConversation | ok | pass | pass | pass |
| ModerationService | MinimizeComment | `UNIMPLEMENTED` | pass | pass | pass |
| ModerationService | ReinstateTakedown | ok | pass | pass | pass |
| ModerationService | ReleaseThrottledPrincipal | ok | pass | pass | pass |
| ModerationService | RemoveInteractionLimit | ok | pass | pass | pass |
| ModerationService | RemoveModerator | ok | pass | pass | pass |
| ModerationService | ReportContent | ok | pass | pass | pass |
| ModerationService | ResolveContentReport | ok | pass | pass | pass |
| ModerationService | SetFirstTimeContributorSettings | ok | pass | pass | pass |
| ModerationService | SetFloodLimits | ok | pass | pass | pass |
| ModerationService | SetInteractionLimit | ok | pass | pass | pass |
| ModerationService | UnblockUser | ok | pass | pass | pass |
| ModerationService | UnlockConversation | ok | pass | pass | pass |
| ModerationService | UnminimizeComment | `UNIMPLEMENTED` | pass | pass | pass |
| NotificationService | CreateNotificationReminder | ok | pass | pass | pass |
| NotificationService | DeleteNotificationReminder | ok | pass | pass | pass |
| NotificationService | DeleteRepoWatch | ok | pass | pass | pass |
| NotificationService | GetNotificationReminder | ok | pass | pass | pass |
| NotificationService | GetNotificationReplyAddress | ok | pass | pass | pass |
| NotificationService | GetNotificationSettings | ok | pass | pass | pass |
| NotificationService | GetNotificationSubscription | ok | pass | pass | pass |
| NotificationService | GetNotificationThread | ok | pass | pass | pass |
| NotificationService | GetNotificationUnreadCount | ok | pass | pass | pass |
| NotificationService | GetRepoWatch | ok | pass | pass | pass |
| NotificationService | IgnoreNotificationThread | ok | pass | pass | pass |
| NotificationService | ListNotificationReminders | ok | pass | pass | pass |
| NotificationService | ListNotificationThreads | ok | pass | pass | pass |
| NotificationService | ListRepoWatchers | ok | pass | pass | pass |
| NotificationService | ListWatchedRepos | ok | pass | pass | pass |
| NotificationService | MarkNotificationThreadsDone | ok | pass | pass | pass |
| NotificationService | MarkNotificationThreadsRead | ok | pass | pass | pass |
| NotificationService | MarkNotificationThreadsUnread | ok | pass | pass | pass |
| NotificationService | SaveNotificationThreads | ok | pass | pass | pass |
| NotificationService | SetRepoWatch | ok | pass | pass | pass |
| NotificationService | StreamNotificationInbox | ok | pass | pass | pass |
| NotificationService | SubscribeNotificationThread | ok | pass | pass | pass |
| NotificationService | UnsaveNotificationThreads | ok | pass | pass | pass |
| NotificationService | UnsubscribeNotificationThread | ok | pass | pass | pass |
| NotificationService | UpdateNotificationReminder | ok | pass | pass | pass |
| NotificationService | UpdateNotificationSettings | ok | pass | pass | pass |
| ObjectService | GetDaemonSnapshot | ok | pass | pass | pass |
| ObjectService | GetObjects | ok | pass | pass | pass |
| ObjectService | GetServerInfo | ok | pass | pass | pass |
| ObjectService | GetTierStats | ok | pass | pass | pass |
| ObjectService | HasObjects | ok | pass | pass | pass |
| ObjectService | ResolveRef | ok | pass | pass | pass |
| OpsService | CreateOpsIncident | ok | pass | pass | pass |
| OpsService | GetEffectiveOpsConfig | ok | pass | pass | pass |
| OpsService | GetOpsCacheStats | ok | pass | pass | pass |
| OpsService | GetOpsClusterStatus | ok | pass | pass | pass |
| OpsService | GetOpsHealth | ok | pass | pass | pass |
| OpsService | GetOpsRollout | ok | pass | pass | pass |
| OpsService | GetOpsTrace | ok | pass | pass | pass |
| OpsService | GetRateLimitStatus | ok | pass | pass | pass |
| OpsService | ListMetricDescriptors | ok | pass | pass | pass |
| OpsService | ListOpsBackups | ok | pass | pass | pass |
| OpsService | ListOpsIncidents | ok | pass | pass | pass |
| OpsService | ListOpsRestoreDrills | ok | pass | pass | pass |
| OpsService | ListOpsRollouts | ok | pass | pass | pass |
| OpsService | ListOpsStatusComponents | ok | pass | pass | pass |
| OpsService | PauseOpsRollout | `INVALID_STATE` | pass | pass | pass |
| OpsService | QueryMetrics | ok | pass | pass | pass |
| OpsService | QueryMetricsRange | ok | pass | pass | pass |
| OpsService | QueryOpsLogs | ok | pass | pass | pass |
| OpsService | ResumeOpsRollout | `INVALID_STATE` | pass | pass | pass |
| OpsService | RollbackOpsRollout | `INVALID_STATE` | pass | pass | pass |
| OpsService | SearchOpsTraces | ok | pass | pass | pass |
| OpsService | SetOpsStatusComponent | ok | pass | pass | pass |
| OpsService | TailOpsLogs | ok | pass | pass | pass |
| OpsService | UpdateOpsIncident | ok | pass | pass | pass |
| OpsService | ValidateOpsConfig | ok | pass | pass | pass |
| OrgService | AcceptOrgInvitation | ok | pass | pass | pass |
| OrgService | AssignOrgRole | ok | pass | pass | pass |
| OrgService | CancelOrgInvitation | ok | pass | pass | pass |
| OrgService | ConvertToOutsideCollaborator | ok | pass | pass | pass |
| OrgService | CreateOrg | ok | pass | pass | pass |
| OrgService | CreateOrgInvitation | ok | pass | pass | pass |
| OrgService | DeclineOrgInvitation | ok | pass | pass | pass |
| OrgService | DeleteOrg | ok | pass | pass | pass |
| OrgService | GetOrg | ok | pass | pass | pass |
| OrgService | GetOrgSettings | ok | pass | pass | pass |
| OrgService | ListMembers | ok | pass | pass | pass |
| OrgService | ListOrgInvitations | ok | pass | pass | pass |
| OrgService | ListOrgs | ok | pass | pass | pass |
| OrgService | ListOutsideCollaborators | ok | pass | pass | pass |
| OrgService | RenameOrg | ok | pass | pass | pass |
| OrgService | RestoreOrg | ok | pass | pass | pass |
| OrgService | RevokeOrgRole | ok | pass | pass | pass |
| OrgService | SetMember | ok | pass | pass | pass |
| OrgService | UpdateOrg | ok | pass | pass | pass |
| OrgService | UpdateOrgSettings | ok | pass | pass | pass |
| PackageService | BeginPackageUpload | ok | pass | pass | pass |
| PackageService | DeletePackage | ok | pass | pass | pass |
| PackageService | DeletePackageVersion | ok | pass | pass | pass |
| PackageService | FinalizePackageVersion | ok | pass | pass | pass |
| PackageService | GetPackage | ok | pass | pass | pass |
| PackageService | GetPackageOwnerSettings | ok | pass | pass | pass |
| PackageService | GetPackageRetentionPolicy | ok | pass | pass | pass |
| PackageService | GetPackageUsage | ok | pass | pass | pass |
| PackageService | GetPackageVersion | ok | pass | pass | pass |
| PackageService | GetPackageVersionProvenance | `NOT_FOUND` | pass | pass | pass |
| PackageService | GrantPackageAccess | ok | pass | pass | pass |
| PackageService | LinkPackageToRepo | ok | pass | pass | pass |
| PackageService | ListPackageAccessGrants | ok | pass | pass | pass |
| PackageService | ListPackageVersions | ok | pass | pass | pass |
| PackageService | ListPackages | ok | pass | pass | pass |
| PackageService | RestorePackage | ok | pass | pass | pass |
| PackageService | RestorePackageVersion | ok | pass | pass | pass |
| PackageService | RevokePackageAccess | ok | pass | pass | pass |
| PackageService | SetPackageRetentionPolicy | ok | pass | pass | pass |
| PackageService | SetPackageVersionTags | ok | pass | pass | pass |
| PackageService | SetPackageVisibility | ok | pass | pass | pass |
| PackageService | UnlinkPackageFromRepo | ok | pass | pass | pass |
| PackageService | UpdatePackage | ok | pass | pass | pass |
| PackageService | UpdatePackageOwnerSettings | ok | pass | pass | pass |
| PagesService | AddPagesVerifiedDomain | ok | pass | pass | pass |
| PagesService | CancelPagesDeployment | `INVALID_STATE` | pass | pass | pass |
| PagesService | CreatePagesDeployment | ok | pass | pass | pass |
| PagesService | CreatePagesSite | ok | pass | pass | pass |
| PagesService | DeletePagesSite | ok | pass | pass | pass |
| PagesService | DeletePagesVerifiedDomain | ok | pass | pass | pass |
| PagesService | GetPagesDeployment | ok | pass | pass | pass |
| PagesService | GetPagesHealthCheck | ok | pass | pass | pass |
| PagesService | GetPagesOrgSettings | ok | pass | pass | pass |
| PagesService | GetPagesSite | ok | pass | pass | pass |
| PagesService | ListPagesDeployments | ok | pass | pass | pass |
| PagesService | ListPagesPreviews | ok | pass | pass | pass |
| PagesService | ListPagesVerifiedDomains | ok | pass | pass | pass |
| PagesService | RequestPagesBuild | ok | pass | pass | pass |
| PagesService | RequestPagesCertificate | `INVALID_STATE` | pass | pass | pass |
| PagesService | SetPagesCustomDomain | ok | pass | pass | pass |
| PagesService | UpdatePagesOrgSettings | ok | pass | pass | pass |
| PagesService | UpdatePagesSite | ok | pass | pass | pass |
| PagesService | VerifyPagesDomain | ok | pass | pass | pass |
| PolicyService | AssignCustomOrgRole | ok | pass | pass | pass |
| PolicyService | CreateCustomRole | ok | pass | pass | pass |
| PolicyService | CreateRuleset | ok | pass | pass | pass |
| PolicyService | DeleteCustomRole | ok | pass | pass | pass |
| PolicyService | DeleteRuleset | ok | pass | pass | pass |
| PolicyService | EvaluatePolicyAction | ok | pass | pass | pass |
| PolicyService | GetCodeowners | ok | pass | pass | pass |
| PolicyService | GetCustomRole | ok | pass | pass | pass |
| PolicyService | GetEffectivePolicy | ok | pass | pass | pass |
| PolicyService | GetPolicyEpoch | ok | pass | pass | pass |
| PolicyService | GetRequiredSignaturesStatus | ok | pass | pass | pass |
| PolicyService | GetRuleset | ok | pass | pass | pass |
| PolicyService | ListBuiltinRoles | ok | pass | pass | pass |
| PolicyService | ListCustomOrgRoleAssignments | ok | pass | pass | pass |
| PolicyService | ListCustomRoles | ok | pass | pass | pass |
| PolicyService | ListPolicyPermissions | ok | pass | pass | pass |
| PolicyService | ListRulesetEvaluations | ok | pass | pass | pass |
| PolicyService | ListRulesetRulesForRef | ok | pass | pass | pass |
| PolicyService | ListRulesets | ok | pass | pass | pass |
| PolicyService | SimulateRuleset | ok | pass | pass | pass |
| PolicyService | UnassignCustomOrgRole | ok | pass | pass | pass |
| PolicyService | UpdateCustomRole | ok | pass | pass | pass |
| PolicyService | UpdateRuleset | ok | pass | pass | pass |
| PolicyService | ValidateCodeowners | ok | pass | pass | pass |
| PolicyService | ValidatePolicy | ok | pass | pass | pass |
| PostService | ApprovePost | ok | pass | pass | pass |
| PostService | BeginPostMediaUpload | ok | pass | pass | pass |
| PostService | ClearChangePost | ok | pass | pass | pass |
| PostService | CreatePost | ok | pass | pass | pass |
| PostService | FinalizePostMedia | ok | pass | pass | pass |
| PostService | GetChangePost | ok | pass | pass | pass |
| PostService | GetPost | ok | pass | pass | pass |
| PostService | LikePost | ok | pass | pass | pass |
| PostService | ListFollowingFeed | ok | pass | pass | pass |
| PostService | ListPostAdoptions | ok | pass | pass | pass |
| PostService | ListRepoPosts | ok | pass | pass | pass |
| PostService | RepostPost | ok | pass | pass | pass |
| PostService | SetChangePost | ok | pass | pass | pass |
| PostService | UndoRepost | ok | pass | pass | pass |
| PostService | UnlikePost | ok | pass | pass | pass |
| PostService | WithdrawPost | ok | pass | pass | pass |
| ProjectService | AddProjectItem | ok | pass | pass | pass |
| ProjectService | AddProjectIteration | ok | pass | pass | pass |
| ProjectService | ArchiveProjectItem | ok | pass | pass | pass |
| ProjectService | ClearProjectItemFieldValue | ok | pass | pass | pass |
| ProjectService | CompleteProjectIteration | ok | pass | pass | pass |
| ProjectService | ConvertProjectDraftItem | ok | pass | pass | pass |
| ProjectService | CopyProject | ok | pass | pass | pass |
| ProjectService | CreateProject | ok | pass | pass | pass |
| ProjectService | CreateProjectChart | ok | pass | pass | pass |
| ProjectService | CreateProjectDraftItem | ok | pass | pass | pass |
| ProjectService | CreateProjectField | ok | pass | pass | pass |
| ProjectService | CreateProjectStatusUpdate | ok | pass | pass | pass |
| ProjectService | CreateProjectView | ok | pass | pass | pass |
| ProjectService | CreateProjectWorkflow | ok | pass | pass | pass |
| ProjectService | DeleteProject | ok | pass | pass | pass |
| ProjectService | DeleteProjectChart | ok | pass | pass | pass |
| ProjectService | DeleteProjectField | ok | pass | pass | pass |
| ProjectService | DeleteProjectItem | ok | pass | pass | pass |
| ProjectService | DeleteProjectStatusUpdate | ok | pass | pass | pass |
| ProjectService | DeleteProjectView | ok | pass | pass | pass |
| ProjectService | DeleteProjectWorkflow | ok | pass | pass | pass |
| ProjectService | DisableProjectWorkflow | ok | pass | pass | pass |
| ProjectService | EnableProjectWorkflow | ok | pass | pass | pass |
| ProjectService | GetProject | ok | pass | pass | pass |
| ProjectService | GetProjectByNumber | ok | pass | pass | pass |
| ProjectService | GetProjectItem | ok | pass | pass | pass |
| ProjectService | GetProjectView | ok | pass | pass | pass |
| ProjectService | LinkProjectRepo | ok | pass | pass | pass |
| ProjectService | LinkProjectTeam | ok | pass | pass | pass |
| ProjectService | ListProjectCharts | ok | pass | pass | pass |
| ProjectService | ListProjectFields | ok | pass | pass | pass |
| ProjectService | ListProjectItems | ok | pass | pass | pass |
| ProjectService | ListProjectIterations | ok | pass | pass | pass |
| ProjectService | ListProjectStatusUpdates | ok | pass | pass | pass |
| ProjectService | ListProjectViews | ok | pass | pass | pass |
| ProjectService | ListProjectWorkflows | ok | pass | pass | pass |
| ProjectService | ListProjects | ok | pass | pass | pass |
| ProjectService | MoveProjectItem | ok | pass | pass | pass |
| ProjectService | QueryProjectChart | ok | pass | pass | pass |
| ProjectService | SetProjectItemFieldValue | ok | pass | pass | pass |
| ProjectService | UnarchiveProjectItem | ok | pass | pass | pass |
| ProjectService | UnlinkProjectRepo | ok | pass | pass | pass |
| ProjectService | UnlinkProjectTeam | ok | pass | pass | pass |
| ProjectService | UpdateProject | ok | pass | pass | pass |
| ProjectService | UpdateProjectChart | ok | pass | pass | pass |
| ProjectService | UpdateProjectDraftItem | ok | pass | pass | pass |
| ProjectService | UpdateProjectField | ok | pass | pass | pass |
| ProjectService | UpdateProjectStatusUpdate | ok | pass | pass | pass |
| ProjectService | UpdateProjectView | ok | pass | pass | pass |
| ProjectService | UpdateProjectWorkflow | ok | pass | pass | pass |
| QueryService | Blame | ok | pass | pass | pass |
| QueryService | CompareRevisions | ok | pass | pass | pass |
| QueryService | Diff | ok | pass | pass | pass |
| QueryService | FindCopies | ok | pass | pass | pass |
| QueryService | GetChangesets | ok | pass | pass | pass |
| QueryService | GetDerived | ok | pass | pass | pass |
| QueryService | IsAncestor | ok | pass | pass | pass |
| QueryService | ListDerivedTypes | ok | pass | pass | pass |
| QueryService | ListTree | ok | pass | pass | pass |
| QueryService | Log | ok | pass | pass | pass |
| QueryService | MergeBase | ok | pass | pass | pass |
| QueryService | ReadPath | ok | pass | pass | pass |
| QueryService | Resolve | ok | pass | pass | pass |
| RefService | CreateRef | ok | pass | pass | pass |
| RefService | DeleteRef | ok | pass | pass | pass |
| RefService | GetRefLog | ok | pass | pass | pass |
| RefService | ListRefs | ok | pass | pass | pass |
| RefService | RenameRef | ok | pass | pass | pass |
| RefService | UpdateRef | ok | pass | pass | pass |
| ReleaseService | BeginReleaseAssetUpload | ok | pass | pass | pass |
| ReleaseService | CreateRelease | ok | pass | pass | pass |
| ReleaseService | CreateRepoTag | ok | pass | pass | pass |
| ReleaseService | DeleteRelease | ok | pass | pass | pass |
| ReleaseService | DeleteReleaseAsset | ok | pass | pass | pass |
| ReleaseService | DeleteRepoTag | ok | pass | pass | pass |
| ReleaseService | FinalizeReleaseAsset | ok | pass | pass | pass |
| ReleaseService | GenerateReleaseNotes | ok | pass | pass | pass |
| ReleaseService | GetLatestRelease | ok | pass | pass | pass |
| ReleaseService | GetRelease | ok | pass | pass | pass |
| ReleaseService | GetReleaseAsset | ok | pass | pass | pass |
| ReleaseService | GetReleaseAssetDownloadUrl | ok | pass | pass | pass |
| ReleaseService | GetReleaseAttestation | `NOT_FOUND` | pass | pass | pass |
| ReleaseService | GetReleaseByTag | ok | pass | pass | pass |
| ReleaseService | GetRepoTag | ok | pass | pass | pass |
| ReleaseService | GetSourceArchiveUrl | ok | pass | pass | pass |
| ReleaseService | LinkReleaseDiscussion | ok | pass | pass | pass |
| ReleaseService | ListReleaseAssets | ok | pass | pass | pass |
| ReleaseService | ListReleases | ok | pass | pass | pass |
| ReleaseService | ListRepoTags | ok | pass | pass | pass |
| ReleaseService | PublishRelease | ok | pass | pass | pass |
| ReleaseService | UpdateRelease | ok | pass | pass | pass |
| ReleaseService | UpdateReleaseAsset | ok | pass | pass | pass |
| RepoService | AcceptRepoInvitation | ok | pass | pass | pass |
| RepoService | AddCollaborator | ok | pass | pass | pass |
| RepoService | AddDeployKey | ok | pass | pass | pass |
| RepoService | ArchiveRepo | ok | pass | pass | pass |
| RepoService | CreateAutolink | ok | pass | pass | pass |
| RepoService | CreateRepo | ok | pass | pass | pass |
| RepoService | CreateRepoFromTemplate | ok | pass | pass | pass |
| RepoService | CreateRepoMirror | ok | pass | pass | pass |
| RepoService | CreateSocialPreviewUpload | ok | pass | pass | pass |
| RepoService | DeclineRepoInvitation | ok | pass | pass | pass |
| RepoService | DeleteAutolink | ok | pass | pass | pass |
| RepoService | DeleteCustomPropertyDefinition | ok | pass | pass | pass |
| RepoService | DeleteDeployKey | ok | pass | pass | pass |
| RepoService | DeleteRepo | ok | pass | pass | pass |
| RepoService | DeleteRepoMirror | ok | pass | pass | pass |
| RepoService | ForkRepo | ok | pass | pass | pass |
| RepoService | GetCollaboratorPermission | ok | pass | pass | pass |
| RepoService | GetContext | ok | pass | pass | pass |
| RepoService | GetRepo | ok | pass | pass | pass |
| RepoService | GetRepoCustomProperties | ok | pass | pass | pass |
| RepoService | GetRepoSettings | ok | pass | pass | pass |
| RepoService | GetResolverHook | ok | pass | pass | pass |
| RepoService | ListAutolinks | ok | pass | pass | pass |
| RepoService | ListCollaborators | ok | pass | pass | pass |
| RepoService | ListCustomPropertyDefinitions | ok | pass | pass | pass |
| RepoService | ListDeletedRepos | ok | pass | pass | pass |
| RepoService | ListDeployKeys | ok | pass | pass | pass |
| RepoService | ListForks | ok | pass | pass | pass |
| RepoService | ListRepoInvitations | ok | pass | pass | pass |
| RepoService | ListRepoMirrors | ok | pass | pass | pass |
| RepoService | ListRepos | ok | pass | pass | pass |
| RepoService | RemoveCollaborator | ok | pass | pass | pass |
| RepoService | RestoreRepo | ok | pass | pass | pass |
| RepoService | SetCustomPropertyDefinition | ok | pass | pass | pass |
| RepoService | SetRepoCustomProperties | ok | pass | pass | pass |
| RepoService | SetRepoTopics | ok | pass | pass | pass |
| RepoService | SetResolverHook | ok | pass | pass | pass |
| RepoService | SyncFork | ok | pass | pass | pass |
| RepoService | SyncRepoMirror | ok | pass | pass | pass |
| RepoService | TransferRepo | ok | pass | pass | pass |
| RepoService | UnarchiveRepo | ok | pass | pass | pass |
| RepoService | UpdateRepo | ok | pass | pass | pass |
| RepoService | UpdateRepoSettings | ok | pass | pass | pass |
| RunnerService | AcquireRunnerJob | ok | pass | pass | pass |
| RunnerService | AddRunnerLabels | ok | pass | pass | pass |
| RunnerService | AppendRunnerJobLog | ok | pass | pass | pass |
| RunnerService | CompleteRunnerJob | ok | pass | pass | pass |
| RunnerService | CreateRunnerCustomImage | ok | pass | pass | pass |
| RunnerService | CreateRunnerGroup | ok | pass | pass | pass |
| RunnerService | CreateRunnerJitConfig | ok | pass | pass | pass |
| RunnerService | CreateRunnerRegistrationToken | ok | pass | pass | pass |
| RunnerService | CreateRunnerRemoveToken | ok | pass | pass | pass |
| RunnerService | DeleteRunner | ok | pass | pass | pass |
| RunnerService | DeleteRunnerCustomImage | ok | pass | pass | pass |
| RunnerService | DeleteRunnerGroup | ok | pass | pass | pass |
| RunnerService | FetchRunnerJobSecrets | ok | pass | pass | pass |
| RunnerService | GetRunner | ok | pass | pass | pass |
| RunnerService | GetRunnerGroup | ok | pass | pass | pass |
| RunnerService | ListRunnerGroupRepos | ok | pass | pass | pass |
| RunnerService | ListRunnerGroups | ok | pass | pass | pass |
| RunnerService | ListRunnerImages | ok | pass | pass | pass |
| RunnerService | ListRunnerMachineClasses | ok | pass | pass | pass |
| RunnerService | ListRunners | ok | pass | pass | pass |
| RunnerService | RegisterRunner | ok | pass | pass | pass |
| RunnerService | RemoveRunnerLabel | ok | pass | pass | pass |
| RunnerService | RenewRunnerJob | ok | pass | pass | pass |
| RunnerService | SetRunnerGroupRepos | ok | pass | pass | pass |
| RunnerService | SetRunnerGroupRunners | ok | pass | pass | pass |
| RunnerService | SetRunnerLabels | ok | pass | pass | pass |
| RunnerService | UpdateRunnerGroup | ok | pass | pass | pass |
| RunnerService | UpdateRunnerJobStep | ok | pass | pass | pass |
| SearchService | CreateSavedSearch | ok | pass | pass | pass |
| SearchService | DeleteSavedSearch | ok | pass | pass | pass |
| SearchService | GetDocumentSymbols | ok | pass | pass | pass |
| SearchService | GetSavedSearch | ok | pass | pass | pass |
| SearchService | GetSymbolDefinitions | ok | pass | pass | pass |
| SearchService | GetSymbolReferences | ok | pass | pass | pass |
| SearchService | GetTopic | ok | pass | pass | pass |
| SearchService | GetTopicCollection | `NOT_FOUND` | pass | pass | pass |
| SearchService | ListFeaturedTopics | ok | pass | pass | pass |
| SearchService | ListLeaderboard | ok | pass | pass | pass |
| SearchService | ListSavedSearches | ok | pass | pass | pass |
| SearchService | ListTopicCollections | ok | pass | pass | pass |
| SearchService | ListTrendingDevelopers | ok | pass | pass | pass |
| SearchService | ListTrendingRepos | ok | pass | pass | pass |
| SearchService | ParseSearchQuery | ok | pass | pass | pass |
| SearchService | SearchChangesets | ok | pass | pass | pass |
| SearchService | SearchCode | ok | pass | pass | pass |
| SearchService | SearchDiscussions | ok | pass | pass | pass |
| SearchService | SearchIssues | ok | pass | pass | pass |
| SearchService | SearchPackages | ok | pass | pass | pass |
| SearchService | SearchRepos | ok | pass | pass | pass |
| SearchService | SearchSymbols | ok | pass | pass | pass |
| SearchService | SearchTopics | ok | pass | pass | pass |
| SearchService | SearchUsers | ok | pass | pass | pass |
| SearchService | StreamSearchCode | ok | pass | pass | pass |
| SearchService | UpdateSavedSearch | ok | pass | pass | pass |
| SecretService | AddSecretSelectedRepo | ok | pass | pass | pass |
| SecretService | CreateVariable | ok | pass | pass | pass |
| SecretService | DeleteSecret | ok | pass | pass | pass |
| SecretService | DeleteVariable | ok | pass | pass | pass |
| SecretService | GetSecret | ok | pass | pass | pass |
| SecretService | GetSecretPublicKey | ok | pass | pass | pass |
| SecretService | GetVariable | ok | pass | pass | pass |
| SecretService | ListRepoOrgSecrets | ok | pass | pass | pass |
| SecretService | ListSecretSelectedRepos | ok | pass | pass | pass |
| SecretService | ListSecrets | ok | pass | pass | pass |
| SecretService | ListVariableSelectedRepos | ok | pass | pass | pass |
| SecretService | ListVariables | ok | pass | pass | pass |
| SecretService | PutSecret | ok | pass | pass | pass |
| SecretService | RemoveSecretSelectedRepo | ok | pass | pass | pass |
| SecretService | SetSecretSelectedRepos | ok | pass | pass | pass |
| SecretService | SetVariableSelectedRepos | ok | pass | pass | pass |
| SecretService | UpdateVariable | ok | pass | pass | pass |
| SecurityService | AddSecurityAdvisoryCollaborator | ok | pass | pass | pass |
| SecurityService | CreateSecretScanningPattern | ok | pass | pass | pass |
| SecurityService | CreateSecurityAdvisory | ok | pass | pass | pass |
| SecurityService | CreateSecurityCampaign | ok | pass | pass | pass |
| SecurityService | DeleteSecretScanningPattern | ok | pass | pass | pass |
| SecurityService | ExportSbom | ok | pass | pass | pass |
| SecurityService | GetAdvisory | ok | pass | pass | pass |
| SecurityService | GetCodeScanningAlert | ok | pass | pass | pass |
| SecurityService | GetCodeScanningAnalysis | ok | pass | pass | pass |
| SecurityService | GetCodeScanningDefaultSetup | ok | pass | pass | pass |
| SecurityService | GetDependencyAlert | ok | pass | pass | pass |
| SecurityService | GetDependencyReview | ok | pass | pass | pass |
| SecurityService | GetDependencyUpdateConfig | ok | pass | pass | pass |
| SecurityService | GetDependencyUpdateRun | ok | pass | pass | pass |
| SecurityService | GetPrivateVulnerabilityReporting | ok | pass | pass | pass |
| SecurityService | GetPushProtectionBypass | ok | pass | pass | pass |
| SecurityService | GetSarifUpload | ok | pass | pass | pass |
| SecurityService | GetSecretScanningAlert | ok | pass | pass | pass |
| SecurityService | GetSecurityAdvisory | ok | pass | pass | pass |
| SecurityService | GetSecurityCampaign | ok | pass | pass | pass |
| SecurityService | GetSecurityConfiguration | ok | pass | pass | pass |
| SecurityService | GetSecurityOverview | ok | pass | pass | pass |
| SecurityService | GetSecurityPolicy | ok | pass | pass | pass |
| SecurityService | ListAdvisories | ok | pass | pass | pass |
| SecurityService | ListCodeQualityFindings | ok | pass | pass | pass |
| SecurityService | ListCodeScanningAlertInstances | ok | pass | pass | pass |
| SecurityService | ListCodeScanningAlerts | ok | pass | pass | pass |
| SecurityService | ListCodeScanningAnalyses | ok | pass | pass | pass |
| SecurityService | ListDependencies | ok | pass | pass | pass |
| SecurityService | ListDependencyAlerts | ok | pass | pass | pass |
| SecurityService | ListDependencyManifests | ok | pass | pass | pass |
| SecurityService | ListDependencyUpdateRuns | ok | pass | pass | pass |
| SecurityService | ListDependents | ok | pass | pass | pass |
| SecurityService | ListPushProtectionBypasses | ok | pass | pass | pass |
| SecurityService | ListSecretScanningAlerts | ok | pass | pass | pass |
| SecurityService | ListSecretScanningLocations | ok | pass | pass | pass |
| SecurityService | ListSecretScanningPatterns | ok | pass | pass | pass |
| SecurityService | ListSecurityAdvisories | ok | pass | pass | pass |
| SecurityService | ListSecurityCampaigns | ok | pass | pass | pass |
| SecurityService | PublishSecurityAdvisory | ok | pass | pass | pass |
| SecurityService | RemoveSecurityAdvisoryCollaborator | ok | pass | pass | pass |
| SecurityService | RequestAdvisoryCve | ok | pass | pass | pass |
| SecurityService | RequestCodeScanningAutofix | `UNIMPLEMENTED` | pass | pass | pass |
| SecurityService | RequestPushProtectionBypass | ok | pass | pass | pass |
| SecurityService | RescanSecrets | ok | pass | pass | pass |
| SecurityService | ReviewPushProtectionBypass | ok | pass | pass | pass |
| SecurityService | SetDependencyUpdateConfig | ok | pass | pass | pass |
| SecurityService | SetPrivateVulnerabilityReporting | ok | pass | pass | pass |
| SecurityService | SetSecretScanningValidityChecks | ok | pass | pass | pass |
| SecurityService | SubmitDependencySnapshot | ok | pass | pass | pass |
| SecurityService | SubmitVulnerabilityReport | ok | pass | pass | pass |
| SecurityService | TriggerDependencyUpdateRun | ok | pass | pass | pass |
| SecurityService | UpdateCodeQualityFinding | ok | pass | pass | pass |
| SecurityService | UpdateCodeScanningAlert | ok | pass | pass | pass |
| SecurityService | UpdateCodeScanningDefaultSetup | ok | pass | pass | pass |
| SecurityService | UpdateDependencyAlert | ok | pass | pass | pass |
| SecurityService | UpdateSecretScanningAlert | ok | pass | pass | pass |
| SecurityService | UpdateSecretScanningPattern | ok | pass | pass | pass |
| SecurityService | UpdateSecurityAdvisory | ok | pass | pass | pass |
| SecurityService | UpdateSecurityCampaign | ok | pass | pass | pass |
| SecurityService | UpdateSecurityConfiguration | ok | pass | pass | pass |
| SecurityService | UploadSarif | ok | pass | pass | pass |
| SecurityService | VerifyObjectSignature | ok | pass | pass | pass |
| SponsorService | CancelSponsorship | ok | pass | pass | pass |
| SponsorService | CreateSponsorTier | ok | pass | pass | pass |
| SponsorService | CreateSponsorship | ok | pass | pass | pass |
| SponsorService | DeleteSponsorTier | ok | pass | pass | pass |
| SponsorService | GetSponsorFiscalHost | `NOT_FOUND` | pass | pass | pass |
| SponsorService | GetSponsorFundingFile | ok | pass | pass | pass |
| SponsorService | GetSponsorPayoutSummary | ok | pass | pass | pass |
| SponsorService | GetSponsorProfile | ok | pass | pass | pass |
| SponsorService | GetSponsorship | ok | pass | pass | pass |
| SponsorService | ListSponsorContentAccess | ok | pass | pass | pass |
| SponsorService | ListSponsorTiers | ok | pass | pass | pass |
| SponsorService | ListSponsorships | ok | pass | pass | pass |
| SponsorService | UpdateSponsorProfile | ok | pass | pass | pass |
| SponsorService | UpdateSponsorTier | ok | pass | pass | pass |
| TeamService | AddTeamMember | ok | pass | pass | pass |
| TeamService | CancelTeamInvitation | ok | pass | pass | pass |
| TeamService | CreateTeam | ok | pass | pass | pass |
| TeamService | DeleteTeam | ok | pass | pass | pass |
| TeamService | GetTeam | ok | pass | pass | pass |
| TeamService | GetTeamMembership | ok | pass | pass | pass |
| TeamService | GetTeamReviewAssignment | ok | pass | pass | pass |
| TeamService | GrantTeamRepoAccess | ok | pass | pass | pass |
| TeamService | LinkTeamExternalGroup | ok | pass | pass | pass |
| TeamService | ListChildTeams | ok | pass | pass | pass |
| TeamService | ListPrincipalTeams | ok | pass | pass | pass |
| TeamService | ListRepoTeams | ok | pass | pass | pass |
| TeamService | ListTeamExternalGroups | ok | pass | pass | pass |
| TeamService | ListTeamInvitations | ok | pass | pass | pass |
| TeamService | ListTeamMembers | ok | pass | pass | pass |
| TeamService | ListTeamRepos | ok | pass | pass | pass |
| TeamService | ListTeams | ok | pass | pass | pass |
| TeamService | RemoveTeamMember | ok | pass | pass | pass |
| TeamService | RevokeTeamRepoAccess | ok | pass | pass | pass |
| TeamService | UnlinkTeamExternalGroup | ok | pass | pass | pass |
| TeamService | UpdateTeam | ok | pass | pass | pass |
| TeamService | UpdateTeamReviewAssignment | ok | pass | pass | pass |
| UserService | AddStarListRepo | ok | pass | pass | pass |
| UserService | AddUserEmail | ok | pass | pass | pass |
| UserService | AddUserSigningKey | ok | pass | pass | pass |
| UserService | AddUserSshKey | `UNIMPLEMENTED` | pass | pass | pass |
| UserService | BeginUserAvatarUpload | ok | pass | pass | pass |
| UserService | ClearUserStatus | ok | pass | pass | pass |
| UserService | CreateStarList | ok | pass | pass | pass |
| UserService | DeleteStarList | ok | pass | pass | pass |
| UserService | DeleteUserAccount | `INVALID_STATE` | pass | pass | pass |
| UserService | DeleteUserEmail | ok | pass | pass | pass |
| UserService | DeleteUserSigningKey | ok | pass | pass | pass |
| UserService | DeleteUserSshKey | `UNIMPLEMENTED` | pass | pass | pass |
| UserService | FinalizeUserAvatarUpload | ok | pass | pass | pass |
| UserService | FollowRepo | ok | pass | pass | pass |
| UserService | FollowUser | ok | pass | pass | pass |
| UserService | GetStarList | ok | pass | pass | pass |
| UserService | GetUser | ok | pass | pass | pass |
| UserService | GetUserActivityOverview | ok | pass | pass | pass |
| UserService | GetUserContributionGraph | ok | pass | pass | pass |
| UserService | GetUserDataExport | ok | pass | pass | pass |
| UserService | GetUserSettings | ok | pass | pass | pass |
| UserService | GetViewer | ok | pass | pass | pass |
| UserService | IsFollowingRepo | ok | pass | pass | pass |
| UserService | IsFollowingUser | ok | pass | pass | pass |
| UserService | IsRepoStarred | ok | pass | pass | pass |
| UserService | ListFollowedRepos | ok | pass | pass | pass |
| UserService | ListRepoFollowers | ok | pass | pass | pass |
| UserService | ListRepoStargazers | ok | pass | pass | pass |
| UserService | ListStarListRepos | ok | pass | pass | pass |
| UserService | ListStarLists | ok | pass | pass | pass |
| UserService | ListStarredRepos | ok | pass | pass | pass |
| UserService | ListUserAchievements | ok | pass | pass | pass |
| UserService | ListUserEmails | ok | pass | pass | pass |
| UserService | ListUserFeedEvents | ok | pass | pass | pass |
| UserService | ListUserFollowers | ok | pass | pass | pass |
| UserService | ListUserFollowing | ok | pass | pass | pass |
| UserService | ListUserPinnedItems | ok | pass | pass | pass |
| UserService | ListUserSecurityEvents | ok | pass | pass | pass |
| UserService | ListUserSessions | `UNIMPLEMENTED` | pass | pass | pass |
| UserService | ListUserSigningKeys | ok | pass | pass | pass |
| UserService | ListUserSshKeys | `UNIMPLEMENTED` | pass | pass | pass |
| UserService | RemoveStarListRepo | ok | pass | pass | pass |
| UserService | RenameUserLogin | ok | pass | pass | pass |
| UserService | RequestUserDataExport | ok | pass | pass | pass |
| UserService | ResendUserEmailVerification | ok | pass | pass | pass |
| UserService | ResolveUserCommitEmails | ok | pass | pass | pass |
| UserService | RevokeUserSession | `UNIMPLEMENTED` | pass | pass | pass |
| UserService | SetPrimaryUserEmail | `INVALID_STATE` | pass | pass | pass |
| UserService | SetUserPinnedItems | ok | pass | pass | pass |
| UserService | SetUserStatus | ok | pass | pass | pass |
| UserService | StarRepo | ok | pass | pass | pass |
| UserService | UnfollowRepo | ok | pass | pass | pass |
| UserService | UnfollowUser | ok | pass | pass | pass |
| UserService | UnstarRepo | ok | pass | pass | pass |
| UserService | UpdateStarList | ok | pass | pass | pass |
| UserService | UpdateUserEmailVisibility | ok | pass | pass | pass |
| UserService | UpdateUserProfile | ok | pass | pass | pass |
| UserService | UpdateUserSettings | ok | pass | pass | pass |
| WikiService | CreateWikiPage | ok | pass | pass | pass |
| WikiService | DeleteWikiPage | ok | pass | pass | pass |
| WikiService | DisableWiki | ok | pass | pass | pass |
| WikiService | EnableWiki | ok | pass | pass | pass |
| WikiService | GetWiki | ok | pass | pass | pass |
| WikiService | GetWikiFooter | ok | pass | pass | pass |
| WikiService | GetWikiPage | ok | pass | pass | pass |
| WikiService | GetWikiSidebar | ok | pass | pass | pass |
| WikiService | ListWikiPageRevisions | ok | pass | pass | pass |
| WikiService | ListWikiPages | ok | pass | pass | pass |
| WikiService | SearchWikiPages | ok | pass | pass | pass |
| WikiService | SetWikiFooter | ok | pass | pass | pass |
| WikiService | SetWikiSidebar | ok | pass | pass | pass |
| WikiService | UpdateWikiPage | ok | pass | pass | pass |
| WorkflowService | ApproveWorkflowRun | ok | pass | pass | pass |
| WorkflowService | CancelWorkflowRun | ok | pass | pass | pass |
| WorkflowService | CommitWorkflowCache | ok | pass | pass | pass |
| WorkflowService | CreateAttestation | ok | pass | pass | pass |
| WorkflowService | CreateRepositoryDispatchEvent | ok | pass | pass | pass |
| WorkflowService | CreateRequiredWorkflow | ok | pass | pass | pass |
| WorkflowService | CreateWorkflowArtifactUpload | ok | pass | pass | pass |
| WorkflowService | CreateWorkflowOidcToken | ok | pass | pass | pass |
| WorkflowService | DeleteAttestation | ok | pass | pass | pass |
| WorkflowService | DeleteRequiredWorkflow | ok | pass | pass | pass |
| WorkflowService | DeleteWorkflowArtifact | ok | pass | pass | pass |
| WorkflowService | DeleteWorkflowCaches | ok | pass | pass | pass |
| WorkflowService | DeleteWorkflowRun | ok | pass | pass | pass |
| WorkflowService | DeleteWorkflowRunLogs | ok | pass | pass | pass |
| WorkflowService | DisableWorkflow | ok | pass | pass | pass |
| WorkflowService | DispatchWorkflow | ok | pass | pass | pass |
| WorkflowService | EnableWorkflow | ok | pass | pass | pass |
| WorkflowService | FinalizeWorkflowArtifact | ok | pass | pass | pass |
| WorkflowService | ForceCancelWorkflowRun | ok | pass | pass | pass |
| WorkflowService | GetActionsPolicy | ok | pass | pass | pass |
| WorkflowService | GetWorkflow | ok | pass | pass | pass |
| WorkflowService | GetWorkflowArtifact | ok | pass | pass | pass |
| WorkflowService | GetWorkflowArtifactDownloadUrl | ok | pass | pass | pass |
| WorkflowService | GetWorkflowBadge | ok | pass | pass | pass |
| WorkflowService | GetWorkflowCacheUsage | ok | pass | pass | pass |
| WorkflowService | GetWorkflowJob | ok | pass | pass | pass |
| WorkflowService | GetWorkflowJobLogsUrl | ok | pass | pass | pass |
| WorkflowService | GetWorkflowJobSummary | ok | pass | pass | pass |
| WorkflowService | GetWorkflowOidcClaimTemplate | ok | pass | pass | pass |
| WorkflowService | GetWorkflowRun | ok | pass | pass | pass |
| WorkflowService | GetWorkflowRunAttempt | ok | pass | pass | pass |
| WorkflowService | GetWorkflowRunLogsUrl | ok | pass | pass | pass |
| WorkflowService | GetWorkflowRunUsage | ok | pass | pass | pass |
| WorkflowService | GetWorkflowUsage | ok | pass | pass | pass |
| WorkflowService | ListActionsPolicyEnabledRepos | ok | pass | pass | pass |
| WorkflowService | ListAttestations | ok | pass | pass | pass |
| WorkflowService | ListRequiredWorkflows | ok | pass | pass | pass |
| WorkflowService | ListWorkflowArtifacts | ok | pass | pass | pass |
| WorkflowService | ListWorkflowCacheUsageByRepo | ok | pass | pass | pass |
| WorkflowService | ListWorkflowCaches | ok | pass | pass | pass |
| WorkflowService | ListWorkflowJobs | ok | pass | pass | pass |
| WorkflowService | ListWorkflowRunApprovals | ok | pass | pass | pass |
| WorkflowService | ListWorkflowRunPendingDeployments | ok | pass | pass | pass |
| WorkflowService | ListWorkflowRuns | ok | pass | pass | pass |
| WorkflowService | ListWorkflows | ok | pass | pass | pass |
| WorkflowService | LookupWorkflowCache | ok | pass | pass | pass |
| WorkflowService | RerunFailedWorkflowJobs | ok | pass | pass | pass |
| WorkflowService | RerunWorkflowJob | ok | pass | pass | pass |
| WorkflowService | RerunWorkflowRun | ok | pass | pass | pass |
| WorkflowService | ReserveWorkflowCache | ok | pass | pass | pass |
| WorkflowService | ReviewWorkflowRunPendingDeployments | ok | pass | pass | pass |
| WorkflowService | SearchWorkflowRunLogs | ok | pass | pass | pass |
| WorkflowService | SetActionsPolicy | ok | pass | pass | pass |
| WorkflowService | SetActionsPolicyEnabledRepos | ok | pass | pass | pass |
| WorkflowService | SetWorkflowOidcClaimTemplate | ok | pass | pass | pass |
| WorkflowService | TailWorkflowJobLogs | ok | pass | pass | pass |
| WorkflowService | UpdateRequiredWorkflow | ok | pass | pass | pass |
| WorkflowService | ValidateWorkflow | ok | pass | pass | pass |
| WorkflowService | VerifyAttestation | ok | pass | pass | pass |
| WorkflowService | WatchWorkflowRun | ok | pass | pass | pass |
| WorkspaceService | CreateWorkspace | ok | pass | pass | pass |
| WorkspaceService | CreateWorkspacePrebuildConfig | ok | pass | pass | pass |
| WorkspaceService | DeleteWorkspace | ok | pass | pass | pass |
| WorkspaceService | DeleteWorkspacePrebuildConfig | ok | pass | pass | pass |
| WorkspaceService | ExportWorkspaceChanges | `EMPTY_CHANGE` | pass | pass | pass |
| WorkspaceService | GetWorkspace | ok | pass | pass | pass |
| WorkspaceService | GetWorkspaceConnection | ok | pass | pass | pass |
| WorkspaceService | GetWorkspaceDotfiles | ok | pass | pass | pass |
| WorkspaceService | GetWorkspaceEgressPolicy | ok | pass | pass | pass |
| WorkspaceService | GetWorkspacePrebuildConfig | ok | pass | pass | pass |
| WorkspaceService | GetWorkspaceUsage | ok | pass | pass | pass |
| WorkspaceService | ListWorkspaceMachineTypes | ok | pass | pass | pass |
| WorkspaceService | ListWorkspacePorts | ok | pass | pass | pass |
| WorkspaceService | ListWorkspacePrebuildConfigs | ok | pass | pass | pass |
| WorkspaceService | ListWorkspacePrebuilds | ok | pass | pass | pass |
| WorkspaceService | ListWorkspaces | ok | pass | pass | pass |
| WorkspaceService | RebuildWorkspace | ok | pass | pass | pass |
| WorkspaceService | SetWorkspaceDotfiles | ok | pass | pass | pass |
| WorkspaceService | SetWorkspaceEgressPolicy | ok | pass | pass | pass |
| WorkspaceService | SetWorkspacePortVisibility | ok | pass | pass | pass |
| WorkspaceService | StartWorkspace | ok | pass | pass | pass |
| WorkspaceService | StopWorkspace | ok | pass | pass | pass |
| WorkspaceService | TriggerWorkspacePrebuild | ok | pass | pass | pass |
| WorkspaceService | UpdateWorkspace | ok | pass | pass | pass |
| WorkspaceService | UpdateWorkspacePrebuildConfig | ok | pass | pass | pass |
| WriteService | CreateChangeset | ok | pass | pass | pass |
| WriteService | Land | ok | pass | pass | pass |
| WriteService | Merge | ok | pass | pass | pass |
| WriteService | Rebase | ok | pass | pass | pass |
| WriteService | Revert | ok | pass | pass | pass |
| WriteService | UploadObjects | ok | pass | pass | pass |

<!-- END GENERATED: three-form conformance matrix -->
