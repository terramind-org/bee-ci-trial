# Workflows (GitHub Actions compatibility)

Status: the engine is implemented in `crates/bee-workflow` (layer 1); the runner service
(§8–§12) in `crates/bee-forge/src/ci`. Scope: PRODUCT §4.14 workflow engine
and §7.2 (match documented GitHub Actions behaviour plus the undocumented behaviour real
workflows rely on). This document says what the engine accepts, how Beehive events become
Actions events, what the plan contains, and where behaviour is an interpretation.

The engine is pure: parsing, triggering, expression evaluation, matrix expansion, the job
graph and the plan are functions of the workflow files, the event and the supplied contexts.
No clock, randomness or hash-map order reaches any output. Containers are not run here; the
runner service executes the plan.

## 1. Files

- A workflow is a `.yml` or `.yaml` file directly in `.github/workflows/` (subdirectories are
  ignored, as on GitHub). The file at `GITHUB_SHA` of the event is used (§5 table).
- YAML 1.2 with GitHub's core schema: `on`, `yes`, `off` are strings; `3.10` unquoted is the
  number `3.1`; `010` is `10`; `0x1F` is `31`. Supported: block and flow collections, plain,
  single- and double-quoted scalars (all escapes), literal and folded block scalars with
  chomping and indentation indicators, comments, `---`/`...`, anchors and aliases. Rejected
  with a positioned error: tags, merge keys (`<<`), multiple documents, duplicate keys, bad
  indentation, unquoted patterns starting with `*` or containing `[`/`]` in flow sequences.
- Every error carries `line:column` of the offending node; all problems in a file are reported
  together, sorted by position. Clients receive `INVALID_ARGUMENT` (subcode `INVALID_FIELD`)
  with payload `path`, `line`, `column`, `diagnostics`; a matrix over 256 jobs is
  `TOO_LARGE` (`limit = matrix_jobs`).

## 2. Syntax

Top level: `name`, `run-name`, `on`, `permissions`, `env`, `defaults.run` (`shell`,
`working-directory`), `concurrency`, `jobs`. Keys are case-sensitive; unknown keys are errors.

`on`: a string, a list, or a map of event to filters. Events (each validated, with its activity
types where it has them): `branch_protection_rule`, `check_run`, `check_suite`, `create`,
`delete`, `deployment`, `deployment_status`, `discussion`, `discussion_comment`, `fork`,
`gollum`, `image_version`, `issue_comment`, `issues`, `label`, `merge_group`, `milestone`,
`page_build`, `public`, `pull_request`, `pull_request_review`, `pull_request_review_comment`,
`pull_request_target`, `push`, `registry_package`, `release`, `repository_dispatch` (free-form
types), `schedule`, `status`, `watch`, `workflow_call`, `workflow_dispatch`, `workflow_run`.

| Filter | Events | Rule |
|---|---|---|
| `types` | events with activity types | Default: all types, except `pull_request`/`pull_request_target` = `opened, synchronize, reopened` |
| `branches` / `branches-ignore` | `push`, `pull_request(_target)`, `merge_group`, `workflow_run` | Not both. PR and merge group match the base branch; `workflow_run` the head branch |
| `tags` / `tags-ignore` | `push` | Not both. Only branches defined → tag pushes skip; only tags → branch pushes skip |
| `paths` / `paths-ignore` | `push`, `pull_request(_target)` | Not both. Not evaluated for tags. No changed files → skip. Over 1,000 commits or unknown diff → run. Only the first 3,000 files are seen |
| `workflows` | `workflow_run` | Required; patterns over workflow names |
| `cron` | `schedule` | 5 POSIX fields in UTC, `*` `,` `-` `/`, month and weekday names; day-of-month OR day-of-week when both restricted |
| `inputs` | `workflow_dispatch` (≤ 25; `string`, `boolean`, `number`, `choice` + `options`, `environment`), `workflow_call` (`type` required: `string`, `boolean`, `number`) | Defaults type-checked; choice default must be an option |
| `outputs`, `secrets` | `workflow_call` | `outputs.<id>.value` may read `jobs` (unknown jobs are errors) |

Filter patterns (`glob.rs`, from the cheat sheet): `*` (no `/`), `**` (anything; `**/` may match
nothing), `?` zero or one of the preceding character, `+` one or more of it, `[...]` sets and
ranges, leading `!` negates, `\` escapes. Patterns apply in order: a later matching negative
excludes, a later matching positive re-includes. Matching is case-sensitive and whole-string.

Jobs: ids match `[A-Za-z_][A-Za-z0-9_-]*` and are unique ignoring case. Keys: `name`, `needs`
(known jobs, no self, no cycles), `if`, `permissions` (`read-all`, `write-all`, `{}` or scopes
`actions`, `artifact-metadata`, `attestations`, `checks`, `contents`, `deployments`,
`discussions`, `id-token` (write/none), `issues`, `models` (read/none), `packages`, `pages`,
`pull-requests`, `repository-projects`, `security-events`, `statuses`), `runs-on` (label, list,
or `{group, labels}`), `environment` (name or `{name, url}`), `concurrency`, `outputs`, `env`,
`defaults.run`, `timeout-minutes` (default 360), `strategy` (`matrix`, `fail-fast` default true,
`max-parallel`), `continue-on-error`, `container` (string or `{image, credentials, env, ports,
volumes, options}`), `services`, `steps`. Reusable-workflow jobs instead take `uses`
(`./.github/workflows/x.yml` or `owner/repo/.github/workflows/x.yml@ref`), `with`, `secrets`
(map or `inherit`), plus `name`, `needs`, `if`, `permissions`, `strategy`, `concurrency`.
`needs.<id>` in a job's expressions must name a listed dependency.

Steps: exactly one of `uses` (`owner/repo[/path]@ref`, `./path`, `docker://image`; no
expressions) or `run`; `with` only with `uses`; `shell` and `working-directory` only with
`run`; `id` valid and unique in the job; `if`, `name`, `env`, `continue-on-error`,
`timeout-minutes`.

Concurrency: string or `{group, cancel-in-progress, queue}`; `queue: max` with
`cancel-in-progress: true` is an error.

## 3. Expressions

`${{ }}` anywhere in a string; the closing `}}` is the first outside a string literal. A value
that is exactly one expression keeps its type (needed for `matrix: ${{ fromJSON(...) }}`,
`runs-on`, `continue-on-error`); mixed text yields a string.

- Literals: `null`, `true`, `false`, numbers (JSON forms, `0x` hex, `0o` octal, `NaN`,
  `Infinity`), `'strings'` with `''` escapes.
- Operators (tightest first): `()`, `[]`, `.`, `!`, `< <= > >=`, `== !=`, `&&`, `||`. `&&`/`||`
  short-circuit and return an operand.
- Comparison: different kinds coerce to numbers (`null`→0, booleans→1/0, strings parsed with
  trimming, `''`→0, invalid→NaN, arrays/objects→NaN); strings compare ignoring case; arrays
  and objects equal only to the same instance; NaN compares false.
- Falsy: `false`, `0`, `-0`, `NaN`, `''`, `null`.
- String conversion: `null`→`''`, numbers with 15 significant digits (`1E+21`, `1E-05`),
  `Array`/`Object` for collections.
- Dereference: missing properties and out-of-range indexes give `null`; names are
  case-insensitive; `.*`/`[*]` filters (`labels.*.name`) apply following dereferences to each
  element.
- Functions (case-insensitive): `contains`, `startsWith`, `endsWith` (case-insensitive),
  `format` (`{N}`, `{{`, `}}`; errors on bad formats), `join` (default `,`), `toJSON`
  (2-space indented), `fromJSON`, `hashFiles` (through a pluggable `FileHasher`), `success`,
  `always`, `cancelled`, `failure`.
- Limits: 21,000 characters and depth 50 per expression.
- `if:` may omit `${{ }}`. A condition that calls no status function becomes
  `success() && (expr)`. Text mixed with `${{ }}` in `if:` is a string, true when non-empty
  (the documented pitfall `${{ false }} && true` is true).
- Context availability is validated per key (GitHub's availability table): for example
  `secrets` is not allowed in `if`, `matrix` not in job `if`, `steps` not in `runs-on`,
  `hashFiles` only in step keys, status functions only in `if`. Violations are
  "Unrecognized named-value" / "Unrecognized function" errors at parse time.

## 4. Matrix, job graph, concurrency

- Matrix: cartesian product with the first key outermost; `exclude` removes combinations
  matching every key of an entry (object values match partially); each `include` entry merges
  into every original combination whose original keys it does not contradict (keys added by an
  earlier include can be overwritten, original values never), otherwise it becomes a new
  combination; combinations created by `include` are never merged into. Keys are
  case-insensitive. An empty vector, an unknown exclude key or more than 256 combinations is an
  error. Display names append ` (v1, v2, ...)` unless the job `name` contains an expression.
- Graph: topological order (ties keep source order); a job starts when its needs finish.
  `success()` = every direct need succeeded and no ancestor failed; `failure()` = an ancestor
  failed; `cancelled()` = the run was cancelled. A failure or skip therefore skips dependents
  unless their condition does not require success. A matrix job fails if any leg failed without
  `continue-on-error`; `fail-fast` cancels unfinished legs; outputs merge in completion order.
- Concurrency: group names compare case-insensitively; one in progress per group; default
  `queue: single` keeps one pending (a newcomer cancels the pending one); `queue: max` keeps up
  to 100 pending in arrival order; `cancel-in-progress` cancels the running and pending members.

## 5. Event mapping

| Beehive event | Actions event (activity) | `GITHUB_REF` | `GITHUB_SHA` | Payload highlights |
|---|---|---|---|---|
| Ref update to `refs/heads/*` or `refs/tags/*` | `push` | the ref | new tip (default-branch tip when deleted) | `ref`, `before`, `after`, `created`, `deleted`, `forced`, `head_commit`, `pusher` |
| Ref created | `create` (plus `push`) | the ref | new tip | `ref` (short), `ref_type`, `master_branch` |
| Ref deleted | `delete` (plus `push`) | default branch | default-branch tip | `ref`, `ref_type` |
| Change request opened / revised / reopened | `pull_request` + `pull_request_target` (`opened` / `synchronize` / `reopened`) | `refs/pull/N/merge` (target: default branch) | test-merge commit (target: default-branch tip) | `action`, `number`, `pull_request{number, html_url, title, state, draft, merged, merge_commit_sha, labels, head{ref, sha, repo.fork}, base{ref, sha}}` |
| Change request landed / closed | same, `closed` (`merged` true / false) | same | same | same |
| Change request edited, labeled, unlabeled, assigned, unassigned, review requested/removed, ready for review, converted to draft, locked, unlocked, enqueued, dequeued, milestoned, demilestoned, auto-land enabled/disabled | same, matching activity | same | same | adds `label` for label activities |
| Review on a change request | `pull_request_review` (`submitted`, `edited`, `dismissed`) | `refs/pull/N/merge` | test-merge commit | `review.state`, `pull_request` |
| Merge-queue entry ready | `merge_group` (`checks_requested`) | queue ref (`refs/heads/gh-readonly-queue/...`) | queue commit | `merge_group{head_sha, head_ref, base_sha, base_ref, head_commit}` |
| Release | `release` (activity) | `refs/tags/<tag>` | tagged commit | `release{tag_name, name, prerelease, target_commitish}` |
| Issue activity | `issues` | default branch | default-branch tip | `issue{number, html_url, title, labels}` |
| Comment on issue or change request | `issue_comment` | default branch | default-branch tip | `issue{number, pull_request?}`, `comment.body` |
| Scheduler minute tick | `schedule` (one run per matching cron) | default branch | default-branch tip | `schedule` (the cron) |
| Manual run | `workflow_dispatch` (target file only) | chosen ref | its tip | `ref`, `workflow`, `inputs` (strings); `inputs` context is typed |
| `repository_dispatch` API | `repository_dispatch` (`event_type` as activity) | default branch | default-branch tip | `action`, `branch`, `client_payload` |
| Workflow run state change | `workflow_run` (`requested`, `in_progress`, `completed`) | default branch | default-branch tip | `workflow_run{id, name, head_branch, head_sha, event, conclusion}` |
| Other webhook-style events | same name (`label`, `watch`, `fork`, `discussion`, ...) | default branch | default-branch tip | caller-supplied fields |

Every payload also has `repository` and `sender`. Triggering rules on top of filters:

- `pull_request` does not run while the change request has merge conflicts
  (`pull_request_target` still does).
- `[skip ci]`, `[ci skip]`, `[no ci]`, `[skip actions]`, `[actions skip]` in the head commit
  message, or a `skip-checks: true` trailer, skip `push` and `pull_request` workflows.
- Events caused by a run's own token start no workflows, except `workflow_dispatch` and
  `repository_dispatch`.
- `workflow_run` chains stop after three levels.
- Dispatch inputs: unknown inputs, missing required inputs, a choice outside its options or a
  non-boolean/non-number value are rejected; defaults fill omitted inputs.

## 6. Plan

`plan_event` returns, per triggered workflow (path order, then cron order), either a failed
entry (invalid file or planning error, with diagnostics) or a run: display name, run name
(`run-name`, else the head commit's first line for `push`, the title for pull requests, else the
workflow name), event, ref, SHA, cron, typed inputs, workflow concurrency and permissions, and
jobs in dependency order. A job is:

- `Instances`: one per matrix leg with name, matrix values, strategy context, `runs-on` labels
  and group, environment (name; `url` left for the runner), job concurrency, effective
  permissions, timeout, `continue-on-error`, container and services (empty images dropped),
  env (workflow then job, job winning), outputs and steps. Each step has its display name
  (default `Run <first script line>` or `Run <action>`), `if` source, script or action with
  `with`, resolved shell with its command template (`bash -e {0}` when unspecified on
  Linux/macOS, `pwsh` on Windows labels, the documented templates for `bash`, `sh`, `pwsh`,
  `python`, `cmd`, `powershell`, or a custom `{0}` template), working directory, env,
  `continue-on-error`, `timeout-minutes`.
- `Call`: one per matrix leg with typed inputs (defaults applied, types checked, unknown or
  missing required inputs and secrets rejected), the secrets mapping or `inherit`, and the
  called workflow's own plan (local files from the snapshot; other repositories through a
  `WorkflowResolver`, else `Unresolved`). Callers' env does not reach callees; the `github`
  context is the caller's. Up to 10 workflow levels and 50 unique reusable workflows.
- `Skipped`: a job without needs whose `if` is false at plan time.
- `Deferred`: pre-run settings (matrix, `runs-on`, name, ...) read `needs` outputs; call
  `plan_job` with the needs' outcomes when they finish.

Expressions over `github` (properties known before the run), `inputs`, `vars`, `matrix`,
`strategy` and supplied `needs` are substituted. Expressions over `secrets`, `env`, `steps`,
`job`, `runner`, status functions, `hashFiles` and runner-only `github` properties
(`github.workspace`, `github.job`, ...) remain as `${{ }}` parts for the runner. Templates keep
substituted text separate from expression parts, so data containing `${{` is never evaluated.

## 7. Known differences and interpretations

No documented behaviour is knowingly different. Where GitHub's documentation is silent the
engine makes these choices, each covered by tests:

- A job that fails with `continue-on-error: true` reports `success` in `needs.<id>.result`.
- `success()` in a job condition requires every direct need to have succeeded and no ancestor to
  have failed; `failure()` looks at every ancestor.
- Workflow and job concurrency groups are evaluated at plan time; a job group that reads `needs`
  defers the job.
- `queue: max` beyond 100 pending cancels the oldest pending member.
- A matrix whose combinations are all excluded is an error rather than a job with no legs.
- Schedules are UTC only; a `timezone` key is rejected. GitHub's at-most-every-5-minutes
  throttling is the scheduler's concern, not the matcher's.
- Payloads carry the fields listed in §5, not every field of GitHub's webhook payloads; an
  expression reading an absent field gets `null`, as it would for any missing property.
- Merge keys (`<<`) and YAML tags are rejected.

## 8. Runner service (`bee-forge` `ci` module)

`WorkflowService` and `RunnerService` are served by `CiServices` over `Ci`
(`crates/bee-forge/src/ci`). Everything lives on the forge surface `ci`; each state change is
one transaction that records its events and audit entry (`docs/spec/forge-core.md` §2.3).

**Discovery.** Workflows are `.yml`/`.yaml` files directly in `.github/workflows/` or
`.beehive/workflows/`, read through the `CiVcs` port at `GITHUB_SHA` of the event (the
reference implementation is `HistoryCiVcs` over `bee-history`). Pushes to the default branch
and `ListWorkflows` refresh the workflow rows (name, triggers, dispatch inputs, cron entries,
blob id, badge URL); files that disappear become `DELETED`. `.beehive/workflows` files are
planned with `plan_workflow` and behave exactly like `.github/workflows` ones.

**Triggers** (the listener `CiListener` on the forge event log):

| forge event | workflow event |
|---|---|
| `ref.updated` | `push` (+ `create`/`delete`); `before` is the last tip CI saw for the ref (none = created); changed paths are the VCS diff from `before` (from the default-branch tip for new refs); `forced` = `before` is not an ancestor of `after` |
| `cr.opened`/`revised`/`reopened`/`closed`/`landed`/`edited`/`labels_changed`/`ready_for_review`/... | `pull_request` and `pull_request_target` activities; head ref `cr-<number>`; check runs are reported on the CR head |
| `merge_queue.entered`/`left` | `pull_request` `enqueued`/`dequeued` |
| `issue.*`, `issue_comment.*`, `cr_comment.created` | `issues`, `issue_comment` |
| `release.*` | `release` |
| other kinds with a GitHub event name | that event (`label`, `milestone`, `discussion`, ...) |
| run completion | `workflow_run` `completed` (chains stop after three levels) |
| `Ci::tick_schedule(minute)` | `schedule`, one run per matching cron; each UTC minute is processed once |
| `DispatchWorkflow` / `CreateRepositoryDispatchEvent` | `workflow_dispatch` (inputs validated, `bad_input`) / `repository_dispatch` (≤ 10 payload keys) |

Events whose principal is a job (`job:<id>`) carry `by_workflow_token`. An invalid workflow
file yields a `startup_failure` run for `push` and `pull_request` events.

**Runs.** `REQUESTED` → `ACTION_REQUIRED` (agent-authored change requests when the policy sets
`agent_runs_require_approval`; fork change requests with `ALL_EXTERNAL` approval) or `PENDING`
(workflow concurrency group busy) → `QUEUED` → `IN_PROGRESS` → `COMPLETED`. A run stores its
event, the workflow sources it uses (local files and prefetched `owner/repo/path@ref` reusable
workflows), its `vars` and numbers, so later planning is a pure function of stored data. Jobs
are expanded when their needs finish (`plan_job` with the needs' outcomes); the graph is
rebuilt from job records on every step (`RunGraph` with finished outcomes replayed).
Reusable-workflow calls become graph scopes; the caller's legs complete with the scope's
result and its `workflow_call` outputs. Run numbers are per workflow; `github.run_id` is a
forge-wide counter.

**Jobs.** One record per matrix leg. Legs beyond `max-parallel` are held; `fail-fast` cancels
unfinished siblings (running legs are asked to stop). Then job concurrency (`PENDING`),
environment protection (`WAITING`, §10), then the queue. Check runs: one per job (named like
the job, `workflow_job_id` set, in a check suite per run attempt), reconciled after every
change and by the tick; annotations come from `::error`/`::warning`/`::notice` lines of the
job log; pushes to the default branch report `post_land` runs (bisect input). Usage: every
finished job records wall time (and billable time for hosted classes, rounded up to minutes)
and emits `workflow_usage.recorded`.

**Cancellation and re-runs.** Cancel completes queued, held and waiting jobs as cancelled, asks
running ones to stop, and lets `always()`/`cancelled()` jobs run; force-cancel completes
running jobs at once and skips everything not started. Re-runs create a new attempt (the
previous one is kept for `GetWorkflowRunAttempt`): all jobs, failed and cancelled jobs plus
their dependents, or one job plus its dependents; other jobs are carried over with their
results. Re-runs are refused after the re-run window (30 days by default) and for runs that
failed to start.

**Concurrency.** Groups (lower-cased) hold runs and jobs; `bee_workflow::concurrency::admit`
decides; members leave when they complete and the oldest pending member starts.

## 9. Runners and the job protocol

- Registration tokens (1 hour) and removal tokens are stored hashed. `RegisterRunner`
  exchanges one for a runner and a credential: a Biscuit for `runner:<id>` allowing no repo
  action, accepted only by the job protocol and only while it is the runner's current
  credential. Default labels: `self-hosted`, the OS (`Linux`/`macOS`/`Windows`) and the arch
  (`X64`/`ARM64`); custom labels on top. JIT configs register ephemeral runners.
- Matching: every `runs-on` label (case-insensitive); repo runners serve their repo; org
  runners serve repos their group admits (all; private and internal; selected), public repos
  only with `allows_public_repos`, selected workflows only when restricted; `runs-on.group`
  must name the runner's group. Each org has a default group created on first use.
- `AcquireRunnerJob` streams assignments (oldest queued job first) and keep-alives. An
  assignment carries a lease (`CiConfig::lease_ms`, default 2 min), the job token, the plan
  (`plan_json`, below), the head, the timeout and a one-time signed secrets URL.
- `RenewRunnerJob` extends the lease and reports cancellation; a lost lease is
  `INVALID_STATE`/`lease_lost`. The tick requeues jobs whose lease lapsed (their log restarts)
  up to `max_requeues` (3) times, then fails them; jobs past `timeout-minutes` complete as
  `timed_out`. Ephemeral runners take one job and are removed when it completes.
- `AppendRunnerJobLog`: ≤ 1 MiB at the stored end (masked with every secret the job can see,
  then stored in the blob store); a repeated offset is a no-op; a gap is `offset_gap`.
  Offsets count stored bytes; continue from the returned `stored_end`.
- `CompleteRunnerJob`: outputs that contain a secret are emptied; the summary is masked and
  cut at 1 MiB.

`plan_json` (version 1) is the resolved `JobInstance`: `name`, `github` (plan-time context),
`inputs`, `vars`, `needs`, `matrix`, `strategy`, `runs_on`, `environment`, `timeout_minutes`,
`continue_on_error`, `container`, `services`, `env`, `outputs`, `steps` (`id`, `name`, `if`,
`kind` `run` with `script`/`shell`/`working_directory`, or `uses` with `action`/`with`, `env`,
`continue_on_error`, `timeout_minutes`), `debug`, `permissions`. Every template is an array of
literal strings and `{"expr": source}` parts, so substituted data is never evaluated again.

**Local runner** (`ci::runner`): `LocalRunner` over a `RunnerApi` (in process:
`InProcessApi`). Each job runs in its own directory; `run` steps go through the planned shell
command with the GitHub environment (`GITHUB_*`, `RUNNER_*`, `CI`, `GITHUB_OUTPUT`/`ENV`/
`PATH`/`STEP_SUMMARY`), workflow commands (`add-mask`, `set-output`, `debug`) and masking of
secrets and the job token; step `if`, `continue-on-error` and `timeout-minutes` behave as on
GitHub; a timed-out or cancelled step's process group is killed. Built-in actions:
`actions/checkout` (the job's repo at the head or a changeset id), `actions/setup-*` (verifies
the tool is on `PATH`, does not install versions), `actions/upload-artifact`,
`actions/download-artifact`, `actions/cache` (`cache/restore`, `cache/save`; saving runs after
the job when the key was not restored exactly). Composite and JavaScript actions run whether
local (`./path`) or fetched from another repository through the action registry (§14). Docker
actions and container jobs fail with "not supported by the local runner".

For local development, `bee-server --dev` enables `[forge.local_runner]` unless a supplied
configuration sets `forge.local_runner.enabled = false`. The server registers a repo-scoped
in-process runner when that repository queues work and executes its `run:` steps in a fresh
workspace under the dev data directory. This is deliberately not a production runner: steps run
as processes on the server host, so `--check-production` rejects the setting and deployments use
registered runners instead.

The dev runner executes workflow steps as the server's OS user with that user's file and network
access; it is not a sandbox. Every repository on this server with a workflow, including an
imported or mirrored repository, can run steps on this machine, so use it only with repositories
you trust. Set `[forge.local_runner] enabled = false` to turn it off under `--dev`; production
uses registered runners.

The development runner accepts `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04`,
`macos-latest`, `linux`, and `self-hosted` on every host. These labels are compatibility
selectors only: they do not provide the named operating system or its preinstalled tools.
### Limits of the in-process runner

- It refuses a job before any non-checkout step runs when the job requests a `container`,
  `services`, or a `uses: docker://...` action, including Docker actions reached by following
  local composite actions and local actions with `runs.using: docker`. Local actions are
  preflighted immediately after an initial checkout; remote actions are not fetched during
  preflight and remain refused when reached. The job log says `this runner cannot run
  containers: <request>`.
- A child process starts with an empty environment. The runner supplies `PATH` (the runner
  process's `PATH`, with action-provided paths prepended), a job-local existing `HOME`, `LANG`,
  `CI`, `GITHUB_ACTIONS`, `BEE_ACTIONS`, the documented `GITHUB_*` and `RUNNER_*`
  variables, then workflow `env`, step `env`, and declared-secret values; host environment
  variables are not inherited.
- A job is limited by its `timeout-minutes` and the runner maximum (60 minutes by default),
  whichever is shorter. Expiry kills the current process group and concludes the job
  `timed_out`.
- JavaScript action `pre`, `main`, and `post` processes use the same remaining job deadline
  and process-group termination path as `run:` steps.
- Artifact, cache, and local-action helper paths are resolved against the canonical job workspace;
  symlinks that resolve outside it fail, while symlinks that resolve inside it remain allowed.
- Whenever the runner is enabled, startup logs a warning; outside `--dev` the warning says it was
  explicitly enabled by configuration and that jobs run as the server OS user with access to its
  files and environment.
- A step's `working-directory` must be relative, must not contain `..`, and after resolving
  symlinks must remain inside the job workspace. Invalid directories fail the step before its
  command starts.
- The per-job workspace, including temporary files and fetched actions, is removed after a
  success, failure, timeout, or runner-task abort unless the runner's debugging
  `keep_workspace` option is set.
- Built-in, composite, and JavaScript actions retain the support described above. Docker and
  other unsupported action runtimes are refused; this runner does not add a general Actions
  runtime or install action dependencies.

## 10. Build services

- **Artifacts and caches**: reserve → upload to a signed URL → finalize/commit (size and
  `sha256:` digest checked, `INTEGRITY` otherwise). Artifact names are unique per run;
  retention defaults to, and is capped by, the policy. Caches are scoped by ref: lookups try
  the job's `GITHUB_REF`, the CR base ref, then the default branch, exact key first, then
  restore-key prefixes (newest); entries beyond the repo limit are evicted least recently
  used, and dropped after `cache_retention_days` without access.
- **Secrets and variables** at org (visibility all, private, selected), repo and environment
  scope. Clients send values as libsodium sealed boxes made with the scope's X25519 public key
  (§12); the server opens them in memory and seals them at rest with the server key
  (ChaCha20-Poly1305 bound to `<scope>/<name>`); no API returns them; a running job redeems them
  once. Jobs see org, then repo, then environment values. Deleting an environment deletes its
  secrets and variables.
- **Environments**: jobs naming one pass the ref policy (all; protected refs = default branch
  and `release/*`; custom branch/tag patterns), required reviewers (prevent-self-review;
  admins may bypass when allowed), then the wait timer. Each gated job has a deployment whose
  statuses follow it (`pending`, `queued`, `in_progress`, `success`/`failure`/`inactive`).
  Environments are created on first use without rules. Custom protection rules name an app;
  a gated job or deployment also waits until every enabled rule's app approved (§12).
- **Job tokens**: a Biscuit minted for `job:<id>`, limited to the repo and the job's lifetime,
  then attenuated to the actions its `permissions` grant: any read → `read`; `contents:
  write` → `commit`, `manage_refs`, `land`; `pull-requests: write` → `review`, `triage`;
  `issues: write` → `triage`; `checks`, `statuses` or `actions: write` → `commit`. An access
  source grants `job:<id>` the write role (read when no scope is write) while the job runs.
  Without `permissions:` the default is `contents: read, packages: read` (policy `READ`) or
  write-all (policy `WRITE`). For runs from fork change requests, every write permission is
  reduced to read unless the repository policy enables `fork_send_write_tokens`.
- **OIDC**: `CreateWorkflowOidcToken` (job token, `id-token: write`) signs GitHub's claim set
  (`sub` = `repo:<owner>/<repo>:ref:<ref>`, `...:environment:<name>` or `...:pull_request`,
  or the claim template) with the `KeyPort` as a compact JWS.
- **Attestations**: in-toto statements (default SLSA provenance) in a DSSE envelope signed by
  the `KeyPort`; verification checks the signature, subjects, signer workflow and source ref.
- **Policies**: org → repo with org enforcement (a repo may not loosen `allowed_actions`,
  SHA pinning, the default token permission, fork secret/token sharing, agent approval or
  retention: `POLICY_DENIED`); `enabled_repos` NONE/SELECTED disables triggers; plans with
  disallowed or unpinned actions are `startup_failure`.
- **Required workflows** from a source repo of the org run on change requests of the
  selected repos (or all), alongside the repo's own workflows. With `enforced` the requirement is
  a land gate (`Ci::required_workflow_gates`, F49-007): the land is refused with
  `workflow:<name>` until the requirement's latest run on the change request's head (or the head
  being landed) completed with `success`; `enforced = false` is evaluate-only and never blocks.

## 11. Interpretations (runner service)

- Post-land bisect runs on the default ref only (where `post_land` check runs are reported).
- A bisect needs a changeset where the check passed within `max_range`; without one the
  automatic session ends `INCONCLUSIVE` at once, and a manual one needs `good`.
- Probe jobs keep job-level `concurrency` groups and environments.
- `pre` scripts of top-level remote actions run at job start; local actions and actions inside
  composite actions run `pre` right before `main`.

- `.beehive/workflows/` is a second workflow directory with identical semantics.
- The first push CI sees for a ref counts as its creation (`create` fires).
- Log offsets count stored, masked bytes (a runner continues from `stored_end`).
- A requeued job's log starts over.
- A value masked with `::add-mask::` stays visible in the echoed `run` script that sets it,
  as on GitHub.
- Required-workflow runs use the path `.beehive/required/<id>/<file>` in the run's snapshot.

## 12. Secret and deployment services

`SecretService` (`ci/vault_service.rs`) and `DeploymentService` (`ci/deploy_service.rs`) are
served by `CiServices` and registered by `CiServices::register` with `WorkflowService` and
`RunnerService`.

**Sealed boxes** (`ci/sealed.rs`). `GetSecretPublicKey` returns the scope's 32-byte X25519 key
and a `key_id`; `PutSecret` takes `ephemeral_pk || crypto_box(value, nonce, scope_pk,
ephemeral_sk)` with `nonce = BLAKE2b-192(ephemeral_pk || scope_pk)` (libsodium
`crypto_box_seal`; RustCrypto `crypto_box` 0.9). Keys come from the `SealedBoxPort`; the
default `DerivedSealKeys` derives each scope's private key as HMAC-SHA256(master,
`bee.ci.sealed-box.v1\n<scope key>\n<generation>`) with the master derived from the CI secret
key, so nothing is stored and every node agrees; `Ci::set_sealed_box_port` wires a KMS instead.
A `key_id` other than the current one is `INVALID_ARGUMENT`/`stale_key_id`; a box that does not
open, or is not UTF-8, is `bad_ciphertext`. `seal`, `seal_with_ephemeral` and `open` are the
client primitives (known-answer vector in `tests/ci_followups.rs`).

| `SecretService` RPCs | repo / environment scope | org scope |
|---|---|---|
| public key, get, list secrets; get, list variables | `commit` (write role) | org admin |
| put, delete secrets; create, update, delete variables | `manage_settings` (admin) | org admin |
| `ListRepoOrgSecrets` | `commit` on the repo | — |
| selected-repo RPCs | `INVALID_ARGUMENT` | org admin |

Environment scopes need the environment to exist (`NOT_FOUND`). Workspace-user and agent
scopes answer `UNIMPLEMENTED`/`served_elsewhere`.

**Deployments** (`ci/deployments.rs`). `CreateDeployment` resolves the ref (a pinned changeset
must be reachable from its tip), requires the required contexts (unset = every check reported on
the changeset) to have passed, the environment's ref policy to allow the ref and, with
`require_green`, the changeset to be green; otherwise `POLICY_DENIED` naming what is missing.
With `deploy_green_tip` (and no pinned changeset) a ref whose tip is not green deploys its newest
green changeset instead (`POLICY_DENIED` naming `green/<ref>` when there is none in 200).
With `auto_merge` a ref behind the default ref is `MERGE_CONFLICT`/`auto_merge_behind` (CI never
writes refs). Environments are created on first use. A deployment to an environment with
required reviewers or enabled protection rules is stored `PENDING` and the call answers
`APPROVAL_REQUIRED` (the deployment is listed by `ListPendingDeploymentReviews`). Statuses
append; `SUCCESS` sets the environment URL and, with `auto_inactive`, marks earlier successful
non-transient, non-production deployments of the environment `INACTIVE`; a deployment still
waiting on protection only accepts failure states. Deleting the active deployment of a
non-transient environment is `INVALID_STATE`. `ReviewPendingDeployment` follows the job rules
(listed reviewer, prevent-self-review, admin bypass); for a job's deployment it reviews that
run's environment. `ReportDeploymentProtectionRuleResult` must come from the rule's app (the
caller's principal id is the app id); the last approval queues the deployment or the job (after
its wait timer), a rejection fails it. `GetDeploymentGreenStatus`: a changeset is green when it
has post-land check runs and every latest one passed; `green_tip` is the newest green changeset
on the ref's first-parent chain (200 back); `bisect_in_progress` reads the bisect table.

**`green/<ref>`** (`ci/green.rs`). `GreenListener` watches `check.updated`: a passing `post_land`
run moves `green/<default ref>` to its changeset when that changeset is green, reachable from the
ref's tip, and the move goes forward (not to an ancestor of the current target, not sideways
while the current target is still on the ref). The write is `CiVcs::set_ref_if`, a plain CAS on
the current target; ports that cannot write refs leave green computed on demand. A ref update of
`green/*` starts no workflows.

**`require_green` for workflow jobs** (`Ci::green_gate`, after the ref policy, before reviewers):
the latest `post_land` runs of the run's changeset reported by anything but the deploying run
decide. A failed one rejects the job (it fails with "... due to environment protection rules:
require_green: ..."); a pending one, or a running bisect of the ref, holds it `WAITING`
(`env_state = green_wait`), re-checked every `CiConfig::green_poll_ms` (10 s) by the CI tick.
The deploying run's own jobs are left to `needs`; when nothing else reports post-land checks the
changeset counts as tested only if the deploying run is a push of the default ref, otherwise the
job is rejected.

| `DeploymentService` RPCs | authorization |
|---|---|
| reads, green status, pending reviews | `read` |
| create deployment, create status, delete deployment | `commit` |
| environments and custom protection rules | `manage_settings` (admin) |
| review a pending deployment | `read` + listed reviewer (or admin with bypass) |
| report a protection rule result | `read` + the rule's app |

## 13. Post-land bisect and revert

`crate::bisect` (SPEC §11.6, AS-CI-02). `BisectListener` watches `check.updated`: a failed
`post_land` run on a changeset of the default ref starts an automatic session unless one runs
for that check, or a found culprit's revert CR is still open. Candidates are the first-parent
changesets after the last one where the check passed (`BisectConfig::max_range`, 256); outcomes
already reported on candidates are used. Automatic sessions first re-run the failing job at
the failing changeset once (a probe with source `confirm`): a pass ends the session `FLAKY`
(`bisect.finished` and `bisect.flaky`, audit `bisect.flaky`) and nothing is reverted; a failure
or a re-run that could not start goes on to bisect. A failing check run starts at most one
session. The plan (`bisect/plan.rs`) is pure: each round probes
up to `parallelism` (default 2) untested candidates evenly spaced in the open interval, so `n`
candidates take at most ⌈log_(p+1) n⌉ rounds; untestable candidates are skipped; contradictory
outcomes or only untestable candidates left end the session `INCONCLUSIVE`. Probes go through
the `BisectProber` port; `CiProber` starts `Ci::start_probe_run`: the failing workflow at the
candidate, as a push to the ref, running only the failing job and what it needs. Probe runs report
no check runs, join no concurrency group, need no approval and start no `workflow_run` chain.
Sessions advance on `workflow_run.completed` and on `Bisector::tick`.

When the first failing changeset is found the session records it and the landed CR that produced
it (`Changes::landed_by`: the landed index `forge_cr_landed` maps every changeset a CR's land
put on the ref to the CR, so a culprit in the middle of a stack names its CR; lands recorded
before the index fall back to the first later changeset some CR landed as its final one, when
that CR's change ids include the culprit's), emits `bisect.found` about that CR (payload: culprit author, failing run, revert), opens the
revert through `Changes::revert` (body links the failing run), requests the culprit author's
review on it and comments on the culprit CR. With the repo setting `auto_revert` the revert lands
at once; when policy refuses, it is put on auto-merge (if allowed), or queued when the repo lands
through the merge queue. Manual sessions (`Bisector::start` with `manual`, or `StartBisect` for a
check no workflow job reports) advance on `Bisector::mark` (pass, fail, skip);
`Bisector::next_candidate` names the changeset to test. `Bisector::cancel` cancels a session and
its probe runs. Sessions are rows of `forge_ci_bisects` (ci migration 2); events `bisect.started`,
`bisect.found`, `bisect.finished`, `bisect.reverted`, `bisect.flaky`; audit `bisect.*`.

`BisectCheckService` serves `CheckService` with `StartBisect` (`commit`), `GetBisect` and
`ListBisects` (`read`), delegating every other RPC to the core `ForgeServices`.

## 14. Action registry (local runner)

`uses: owner/repo[/path]@ref` actions are fetched once per job through `RunnerConfig::actions`
(`ci/runner/registry.rs`). `ForgeActionSource` reads them through the CI VCS port: `owner/repo`
as a Beehive repo, else, through `with_mirror(github_owner, org)` (or `*`), the repo of that name
in a Beehive org that holds github.com actions imported by the importer. The ref is a changeset
id, branch or tag. Public repos and repos of the job's org may be used; others are
`PERMISSION_DENIED`. Fetched trees live under the job directory (`_actions/<owner>/<repo>/<ref>`).

- Composite actions run their steps in the job with `inputs`, `github.action_path`,
  `GITHUB_ACTION_PATH`, `GITHUB_ACTION_REPOSITORY` and `GITHUB_ACTION_REF`.
- JavaScript actions (`runs.using: node*`) need `node` on `PATH`. `runs.main` runs in the
  workspace with every input as `INPUT_<NAME>` (upper case, spaces as `_`; defaults evaluated,
  missing required inputs fail the step) and the `GITHUB_OUTPUT`, `GITHUB_ENV`, `GITHUB_PATH`,
  `GITHUB_STATE` and `GITHUB_STEP_SUMMARY` files plus workflow commands (`save-state` included).
  `runs.pre` of top-level remote actions runs before the first step (other actions run it
  right before `main`) and `runs.post` after the last step, in reverse order (`pre-if`/`post-if`, default `always()`); state saved by `pre` and `main` reaches
  later scripts as `STATE_<name>`. A failing `pre` or `post` fails the job.
- Docker actions are rejected ("Docker actions are not supported by the local runner").

## 15. API gaps

Server wiring (`crates/bee-server/src/forge`): `SecretService` and `DeploymentService` are
registered with `WorkflowService` and `RunnerService`; `BisectCheckService` replaces the core
`CheckService`; a `Bisector` is attached next to `Ci::attach` and ticked every 5 s; `/_ci/*`
is routed to `Ci::serve_transfer` (`ci/transfer.rs`: one-time job secrets, artifact and cache
upload/download, job and run logs, each after `UrlSigner::verify`). Still for the server to
choose: `RunnerConfig::actions` for in-process runners (it runs none today), a KMS through
`Ci::set_sealed_box_port`, and `Bisector::set_prober` for external CI.


- Annotations are derived from the log; a runner cannot attach them directly.
- The stock `actions/runner` protocol and the Actions cache/artifact HTTP APIs are not served;
  native runners use the job RPCs.
- Docker actions, container jobs and service containers need a Docker-capable runner; the
  local runner rejects them.
- `ListDeploymentProtectionRuleApps` needs the enterprise apps installations
  (`UNIMPLEMENTED`/`served_elsewhere`); a protection rule stores the app id as its slug.
- API deployments ignore the environment wait timer (it delays workflow jobs only); there is no
  scheduler for API deployments.
- Remote runners need their own `ActionSource` (the action registry has no RPC; proposed:
  `RunnerService.FetchRunnerAction` streaming the tree of an action ref under the job token).
- Artifacts and caches still travel over the `/_ci/*` URLs only (secrets have
  `FetchRunnerJobSecrets` too).

Closed (2026-10-19): `RunnerService.FetchRunnerJobSecrets` (spends the same one-time grant as
`secrets_url`), `WorkflowService.SearchWorkflowRunLogs`, `GetWorkflowBadge` (text and SVG),
`CompleteRunnerJobRequest.environment_url` (on the final deployment status),
`WorkflowRun.numeric_id`, and `CheckService.GetBisectSession`, `GetBisectNextCandidate`,
`MarkBisect`, `CancelBisect` with `StartBisectRequest.manual`.
