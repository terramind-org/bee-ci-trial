# Agents and workspaces (`bee-forge::agents`)

The agent registry, agent runs and their sessions, repo instructions, memory, context
bundles, component watchers with the anomaly feed, automatic agent review, and cloud
workspaces. Contract: PRODUCT §4.19 (and §4.15 "assigning an issue to an agent starts that
agent's work"), SPEC §9.1–§9.2 (principals, delegated budgeted tokens), §10.4 (provenance),
§11.5 (leases). Conventions: `docs/spec/forge-core.md`. Serves `AgentService` (54 RPCs) and
`WorkspaceService` (25 RPCs). Swarms use the same run engine and have their own
`SwarmService` (§10).

## 1. Layout

| path | owns |
|---|---|
| `agents/mod.rs` | `MODULE` (surfaces `agents` and `workspaces`, migrations through `agents/0004`), event kinds, `Agents`, `AgentsConfig`, `AgentPorts`, workers |
| `agents/model.rs` | crate-private records |
| `agents/ports.rs` | `AgentRunner`, `RunTokenIssuer` (+ `KeyRingIssuer`), `AgentBlobs` (+ `MemoryBlobs`), `AgentRepo` (+ impl for `HistoryVcs`), `WorkspaceProvider` |
| `agents/registry.rs` | runtimes, agent definitions, owner budgets, principal summaries, dashboards, work items, activity stream |
| `agents/runs.rs` | run start, dispatch queue, execution, session events, cancel, retry, steering, sessions |
| `agents/triggers.rs` | the event listener and the issues module's `AgentGate` |
| `agents/feed.rs` | feed triggers: followed repos' posts start `ADOPT_EVALUATE` runs (§5) |
| `agents/context.rs` | instructions, memory, context bundles |
| `agents/watchers.rs` | watchers, anomalies, review configuration |
| `agents/workspaces.rs` | workspaces, prebuilds, ports, dotfiles, egress, usage, org workspace policy |
| `agents/devcontainer.rs`, `cron.rs` | `devcontainer.json` (JSONC) parser; five-field cron |
| `agents/local.rs`, `fake.rs` | `LocalProvider` (working-copy directories), `FakeRunner` (deterministic runner) |
| `agents/service.rs` | the two tonic services (`AgentApi`) |
| `swarms/` | `SwarmService`, admission, tasks, membership, and repository work (SPEC §11.10) |

Wiring (like `Changes` and `Enterprise`): the server builds `Agents::new(forge, changes, ports,
config)`, calls `attach()` (event listener + agent gate), `register(routes)` and
`spawn_workers()` (dispatcher, maintenance). Tests call `dispatch_once()` and `maintain()`.

## 2. Registry

- **Runtimes** (`AgentRuntime`): per org, unique `<name>@<version>`; public ones (org `""`)
  only through `Agents::register_runtime` by the operator. Deleting one with active runs is
  `INVALID_STATE`.
- **Agent definitions** (`AgentDefinition`): bind an agent principal (created by
  `AuthService.CreateAgent`) to an org: name, runtime (registered id or a name such as
  `claude-code`, `codex`, custom), model, instructions file path, toolsets (policy actions the
  run token may carry; `purge`, `redact`, `manage_policy` are refused), default per-run
  budget, owners (primary first; the agent's directory owner must be among them),
  purpose, enabled, schedules (cron + repo + prompt). Only agents with an enabled definition
  take work.
- **Owner budgets**: per org and owner, monthly (UTC) limits on USD, input and output tokens
  and runs. Spend is charged once per run when it settles (and one run when it starts).

## 3. Runs

Lifecycle `QUEUED → PROVISIONING → RUNNING → SUCCEEDED | FAILED | CANCELLED |
WAITING_FOR_REVIEW`; a waiting run (it opened CRs) becomes `SUCCEEDED` when its CRs land, `FAILED`
(`change_closed`) when they all close unlanded. Every transition is a session event
(`STATE_CHANGED`), a forge event (`agent_run.*`) and an audit entry (`agent.run_*`).

**Triggers** (`Trigger`): manual (`StartAgentRun`), issue assignment
(`issue.assigned_to_agent`, using the run id the issues module assigned), `@mentions` in issue
and CR comments (issue, review or revise task), automatic review config and agents requested
as reviewers, "request changes" on a waiting run's CR (revise run), component watchers
(debounced), feed triggers (`post.published` in a followed repo, debounced: an `ADOPT_EVALUATE`
run, §5), schedules, retry. Event-driven starts carry an idempotency key derived from the
event subject (the log delivers at least once).

**Tokens.** Each run gets its own token from the `RunTokenIssuer`:

- started through the API: the caller's bearer token is attenuated with `act_as` = the agent,
  so the run can never exceed the caller; chain = `[agent, caller, caller's delegators…]`;
- started by an event: a token minted for the agent on behalf of its owner and the human whose
  action caused the event; chain = `[agent, owner, actor, …]` (deduplicated).

Both are limited to the repo, the run's refs (default `ws/<agent>/*` and the base ref), its
read/write paths, the definition's toolsets ∩ the requested actions (asking for more is
`PERMISSION_DENIED`/`scope_too_broad`; a task whose action is missing is `scope_too_narrow`),
a `Budget` with the run's changeset and land limits, the run's session id, and an expiry. The
token is stored only while the run is live (`forge_agent_run_tokens`, never returned by an RPC)
and revoked when the run ends.

**Budgets.** A run refuses to start (`BUDGET_EXHAUSTED`, subcode `owner_budget_exhausted`)
when the owner's budget is spent: API starts get the error (audited `agent.run_refused`);
event starts record a `FAILED` run and end the issue's run; the issues module's gate refuses
the assignment itself. A run's budget is clamped to the definition's default; a runner whose
reported cost or changeset count exceeds it is cancelled and the run fails with
`BUDGET_EXHAUSTED`; `max_duration_seconds` bounds execution (`DEADLINE_EXCEEDED`).
`usage.recorded` events (`agent_tokens`, `workspaces_hours`) feed billing.

**Execution.** The dispatcher claims queued runs (CAS on the queue row), builds the job
(definition, runtime, resolved instructions plus the definition's file, issue or CR, context
bundles, a workspace when a machine type was asked for, queued steering messages), records the
provenance session (session id, model, runtime, BLAKE3 prompt digest, chain), and calls the
runner. Runner events stream into the session log; the outcome stores log and transcript
through `AgentBlobs` (a `LOG` event with `tool_name = "transcript"` carries the references),
links CRs to the issue (`IssueLinkSource::AGENT_RUN`, closing on land), and records session
links `cs:<hex>` and `cr:<id>` so `sessions_for_change` answers "which session produced this".

## 4. Context

Instructions resolve in precedence order (most specific last): org instructions (org-scope
memory key `instructions`), `.github/copilot-instructions.md`,
`.github/instructions/*.instructions.md` whose `applyTo` matches, then per directory from the
root `AGENTS.md` (or `CLAUDE.md` where present for runtime `claude-code`), then the custom agent
from `.github/agents/`. `@path` lines include a file once. Memory: per repo, org or agent;
versioned with `expected_version`, TTL, tags, deterministic term-match search. Bundles:
private unless shared; materialized per item with `ItemStatus` (subjects in repos the caller
cannot read are `PERMISSION_DENIED`).

## 5. Watchers, anomalies, review config

A watcher (repo maintainers create it with one of their agents) covers paths; landed CRs and
default-ref updates touching them fold into one trigger per watcher (trailing debounce of
`check_interval_seconds`, default 60 s, capped at ten windows), then a free-form run with the
changed paths. Daily USD budgets per watcher skip runs once spent. Anomalies carry a per-repo
sequence that moves on every update (streams resume with `after_seq`); an open anomaly with
the same fingerprint is updated; `auto_open_issues` opens an issue (author = the watching agent)
with the evidence rendered as Markdown.

### Feed triggers (`agents/feed.rs`)

An agent follows repos and evaluates the upgrades they post (`docs/spec/forge-community.md` §9).

- **Config.** `SetAgentFeedTrigger` (create or replace; one trigger per agent and repo) needs a
  repo maintainer (`manage_settings` at the maintain role) who is the agent or one of its owners,
  for an agent enabled in the repo's org (`no_agent`). Settings: enabled, `daily_usd_budget`
  (0–1,000,000, 0 = unlimited; `invalid_budget`), `max_runs_per_day` (0 = unlimited), and at most
  100 `sources`, each a repo the agent can read (`source_not_readable`, also for a missing repo)
  and never the trigger's own repo (`source_is_target`). The agent principal follows each source
  as itself (`FollowRepo`); a source dropped from the set is unfollowed. `GetAgentFeedTrigger`
  and `ListAgentFeedTriggers` (by agent id) need repo `read`; reads fill `runs_today`,
  `spent_usd_today` (current UTC day), `check_interval_seconds` and the sources' current names.
  Events `agent_feed_trigger.updated`, audit `agent.feed_trigger_set`.
- **What fires.** `post.published` in any repo the agent follows (not only the sources) reaches
  every enabled trigger of that agent outside the post's repo, provided the agent may read the
  post's repo. The post is folded into the trigger's pending run: trailing debounce of
  `check_interval_seconds` (the server's `forge.agent_debounce_ms`, default 60 s), capped at
  ten windows after the first post, at most 20 posts per run. A post is taken once per agent and
  repo (`forge_agent_feed_seen`), so a redelivered event starts nothing.
  A post of its own (no change request) fires like any other: the run's prompt takes the diff
  summary of the first change request attached to it, or says that the post has none.
- **The run.** When the window passes the trigger fires (`agent_feed_trigger.fired`, audit
  `agent.feed_trigger_fire`): once the day's run cap or budget is spent it is skipped (the event
  says why) and starts nothing; otherwise it counts a run and starts one `ADOPT_EVALUATE` run in
  the trigger's repo (`task.post_id` = the first post, trigger `feed`, idempotency key
  `adopt:<repo>:<agent>:<post>`, on behalf of whoever configured the trigger, budget the day's
  remaining USD). The run's token is the normal event-started token, limited to that repo; a
  run's cost is charged to the trigger's day.
- **Context.** For every folded post the run's chain can read: its text, its media as URLs
  signed for the run's chain (re-authorized when redeemed), its change number and title, the
  source change's diff summary (paths changed, deleted, diff classes) and changed paths (at most
  200 listed), then the target repo and default ref. A run none of whose posts is still
  published and readable fails `INVALID_ARGUMENT` / `post_not_readable`.
- **Instructions and outcome.** Decide whether the upgrade applies. If not, open nothing and
  finish with the summary `not relevant: <reason>`; if it does, open an issue (or a change
  request for a clear, small port) with `inspired_by_post_id` set, and finish with
  `opened issue #<n>: <title>` or `opened change request #<n>: <title>` (`feed::Outcome`; the
  web reads `AgentRun.summary` in exactly these forms). The `FakeRunner` opens an issue titled
  after the post's change, or finishes "not relevant" when scripted to.

## 6. Workspaces

Created from repo + ref; the devcontainer file (default `.devcontainer/devcontainer.json`;
missing default → a default image, missing explicit path → `INVALID_ARGUMENT`) is parsed with
comments and trailing commas; `hostRequirements` pick the cheapest fitting machine type or
reject a too-small one (`machine_too_small`). Org workspace policy (allowed machine types and
regions, public ports, idle-timeout cap, per-owner quota) → `POLICY_DENIED` / `QUOTA_EXCEEDED`.
States `PROVISIONING → AVAILABLE ⇄ STOPPED`, `REBUILDING`, `FAILED`; delete is refused with
unexported changes unless forced. Export commits the working copy's edits onto
`ws/<owner>/<name>` (`EMPTY_CHANGE` when nothing changed) and optionally opens a CR. Idle
workspaces stop after `idle_timeout_seconds`, stopped ones are deleted after their retention;
usage lines record core-seconds, storage byte-hours and cost. Prebuilds: manual, every land,
devcontainer change, or cron; the newest `retained_versions` stay `READY`, older ones
`SUPERSEDED`. Connection tokens are random, stored as SHA-256, short-lived
(`Workspaces::verify_connection_token` for the server's proxy). Egress: effective policy denies
when any scope denies, intersects allow-lists, unions deny-lists; a narrower scope that would
allow more is `POLICY_DENIED` (`egress_wider_than_scope`).

## 7. Authorization

| RPCs | needs |
|---|---|
| runtimes: register, update, delete | org owner or admin |
| runtimes: get, list; principals, dashboards, work items | org member (public runtimes: any caller) |
| start a run | `commit` (`review` for review tasks) and being the agent, an owner, or a maintainer |
| cancel, retry, steer | `commit` and being the starter, the agent, an owner, or a maintainer |
| run reads, instructions, watcher/anomaly/review-config reads | `read` |
| repo memory and bundles: write | `commit` (changing another's bundle: maintainer) |
| org memory: write | org owner/admin; agent memory: the agent or its owners |
| watchers, review config: write | `manage_settings`, maintain |
| feed triggers: write | `manage_settings` at the maintain role, and being the agent or an owner |
| feed triggers: read | `read` |
| report anomaly | the watcher's agent, or `commit` |
| acknowledge, link anomaly | `triage` |
| workspaces: create | `read` (agent-owned: owning the agent) |
| workspace operations | owner, an owner of the owning agent, or a maintainer (others: `NOT_FOUND`) |
| export | as above plus `commit` on the workspace ref |
| prebuild configs: write, trigger | `manage_settings` |
| dotfiles | the principal or an owner of the agent |
| egress: set | org owner/admin, repo maintainer, or workspace owner per scope |
| usage | own usage (org member), else org owner/admin |

Denials of state-changing calls are audited through `guarded`.

## 8. Ports the server backs

| port | production implementation |
|---|---|
| `AgentRunner` | the sandboxed runner fleet: runs the runtime's image under the workspace egress policy with only the run token; cancel and steer |
| `RunTokenIssuer` | `KeyRingIssuer` over the server's signing key ring and its persistent revocation store |
| `AgentBlobs` | the blob store (logs, transcripts) |
| `AgentRepo` | the server's VCS: revision resolution, reads, diffs, workspace changesets through `CreateChangeset` (push protection → `SECRET_DETECTED`, provenance), ref updates |
| `WorkspaceProvider` | the container/VM fleet (devcontainer build, prebuild images, ports, SSH/web IDE endpoints) |

## 9. API gaps

- No RPC for the org workspace policy; Rust API only.
- `EventService.Subscribe` has no principal filter; `ListAgentActivity` filters by agent and
  delegation root (paged by sequence, not streamed).
- `StartAgentRunRequest.agent_principal_id` empty means "the caller's default agent"; creating
  one on the fly needs `AuthService.CreateAgent`, so a caller without an agent gets
  `NOT_FOUND`/`no_default_agent`.
- `ExportWorkspaceChangesRequest.provenance` is recorded in the `workspace.exported` event; the
  changeset's provenance carries the principal chain only until `AgentRepo::commit` takes
  claims.
- Context bundle subjects resolve for issues and CRs only; other kinds are `NOT_FOUND`.
- `ListAgentPrincipals`, `ListAgents` and work items page by offset; dashboards and stats scan
  up to 10 000 rows per agent.

Closed (2026-10-19): agent definitions (`RegisterAgent`, `GetAgent`, `ListAgents`,
`UpdateAgent`, `DeleteAgent`; owners or org admins write, members read), owner budgets
(`SetAgentBudget` for org admins, `GetAgentBudget` with this month's spend), sessions by change
(`GetSessionsForChange`), the activity stream (`ListAgentActivity`, events of unreadable repos
dropped), `AgentRun.transcript_ref` / `log_ref` / `trigger` / `trigger_ref`, and
`CreateComponentWatcherRequest.agent_principal_id`.

## 10. Swarms

**Record.** The lead's owner creates a swarm in planning from an existing active run whose
`base_ref` is the requested branch. The branch and any issue number must resolve in the same
repository. There is one accountable person, taken from the run at creation. A run has at most
one membership, including after its swarm ends. The user interface names the lead “Queen · lead”.
Only the lead's issued run credential or a descendant may set the status line. A person with
`commit` on the branch can send a message, and it goes into the lead's existing steering channel.

**Tasks.** The lead and its owner arrange work. The assigned member run, lead or owner can
record an attempt. It identifies the member run and agent, carries start and end server
timestamps, and ends with one of finished, empty, or refused. Finishing takes a one-line
reason and a hand-back with done, deviations, concerns, and follow-ups. `FinishSwarmTask` also
requires the current 1-based attempt number. A delayed retry cannot finish a later attempt.
An empty or refused task can be tried again. Claimed paths compare by whole path components.
An attempt starts only while the swarm is working and the run is active. It needs an
unassigned run and paths that do not overlap another active task's claims in the swarm.
If the member run ends with an active attempt, the terminal transition and path release
commit together. An attempt with a recorded hand-back is finished. Otherwise it comes
back empty with the run's failure or cancellation reason as one line, when available.
Path leases remain available to protect the corresponding Bee writes. A hand-back has an
8 KiB encoded limit. The 512 KiB swarm record limit reserves space to close each active
attempt and pending join.

**Joins.** A signed-in repository reader can offer an agent they control and a positive USD
budget, even without membership in a public repository. The requester's token must allow
`swarm_join` on the shared branch. The lead, its owner, or a repository admin may decide.
The requester and joining agent receive `PERMISSION_DENIED/swarm_join_self_decision` if
they try to decide. If the request came from a non-human principal, the lead agent receives
`PERMISSION_DENIED/swarm_join_human_required` when accepting. The accountable person or
a human repository admin can accept that request. Acceptance
checks the same effective policy as the rest of the forge at the default branch, for
`swarm_join` across the approver and applicant chains. The action is repository-wide and
is included in the built-in Read role. An entrant must have an
enabled agent definition in the repository's organization. Its run is charged to the agent's
owner under the ordinary limits. The grant is limited to the repository, branch and worker's
workspace ref. Admission does not give the worker a repository role or exempt its actions
from policy. A policy refusal returns the persisted join with state `REFUSED` and the reason.

**State.** The accountable person or a repository admin changes swarm state. A pause prevents
queued members from starting and holds new attempts. A run already in flight can finish its
attempt. Resuming releases the queue. If a Safety tripwire paused the swarm, resume returns
`INVALID_STATE/swarm_attention_open_<attention_id>` until the answerable person uses
`AcknowledgeSafetyAttention`. The subcode includes the lowercased attention id. A person's
own pause can be resumed directly. Stopping atomically cancels live members through the
run cancellation path, ends active attempts, and closes pending joins. Workers admitted
while planning queue until the swarm moves to working. Finishing requires
every member run to be terminal and every task to have ended. Stopped and finished records are
terminal.

**Reads.** `GetSwarm` and `ListRepoWork` need repository Read. Budget and cost are derived
from member runs. Budget fields are added with saturation, with 0 (unlimited) prevailing if any
member has an unlimited field. Views carry swarm budget and cost for the lead, owner, and repo
admins. Join applicants can see their own offers, and the lead and owner can see all offers.
`ListRepoWork` combines swarms and independent runs and orders them by newest activity with the
record id as a tiebreaker. Each swarm row represents all its members. A page can end early
to stay within the response size limit. Outcome fields are sorted unique pull request ids
resolved from terminal runs' durable pull request numbers, session links, and pull requests
opened with a verified run credential. CLI and MCP use the descriptors of
`proto/bee/v1/swarms.proto`.

**Storage and events.** Migration `agents/0004` adds `forge_swarms` and `forge_swarm_runs`
on the agent surface for SQLite and Postgres. The first holds the swarm record. The second
indexes run membership by repository and run id. Migration `changes/0006` indexes pull
requests by run id. `OpenChange` stores that link in the same transaction as the pull request
after matching the caller's issued token ancestry to the run. Admission and cancellation
update the run queue and swarm on the same surface. Swarm mutations append `swarm.*` events
with the swarm subject and corresponding audit entries. Steering retains the existing `agent_run.message`
event. Agent runs retain their own session and forge events while in a swarm.
