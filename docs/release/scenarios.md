# Agent scenario results: Claude Code

PRODUCT §3.1 says a feature is done when fresh agents pass its scenarios 3/3 on two runtimes. This
is the first published run of the §4 scenarios. It covers **one runtime, Claude Code**; Codex is
pending until its quota resets on 2026-11-17. The two-runtime bar is therefore **not met** for any
scenario, and only 15 of the 39 scenarios reach 3/3 on Claude Code.

## Run

| | |
|---|---|
| Date | 2026-11-03 (UTC) |
| Server | `bee-server` release build from `bcf5f2db2` (swarm branch base); `bee-server --dev --temp` on `127.0.0.1:17420`, one server for the whole session |
| Harness | `harness/agents` at the commits of this branch (the harness fixes listed below are part of it) |
| Runtime | `claude-code` 2.1.304 (`~/.bun/bin/claude`), `--model claude-sonnet-5-5`, `--dangerously-skip-permissions` |
| Concurrency | `--parallel 4` |
| Scenarios | 39: PRODUCT §4 lists 39, and `AS-CR-04` was added by the CI task (design note C1). The task brief said 38. |
| Result | 72 recorded runs, 51 passed. 15 scenarios 3/3, 4 partly passing, 15 never passed, 5 could not run |

Reproduce: `BEE_HARNESS_ADMIN_TOKEN=<dev root token> bun run harness --server <url> --runtimes claude-code --runs 3 --scenarios all --parallel 4 --no-isolate --model claude-sonnet-5-5 --claude-bin <claude> --bee-bin target/release/bee`. In `--dev` the server prints the root token (`token:`) on start and writes it to `<data-dir>/credentials`; the harness admin token is that token.

### Isolation: the agents were not isolated

The runner’s isolate mode points `CLAUDE_CONFIG_DIR` at a fresh directory and needs an API key,
which this machine does not have (Claude Code is signed in through the keychain). The runs used
`--no-isolate`, so **the agents could see the machine’s global Claude config**. What that holds:

- `~/.claude/CLAUDE.md` does **not** exist, so there were no user-level instructions.
- `~/.claude/settings.json` sets a preferred model, `includeCoAuthoredBy`, and a few enabled plugins/marketplaces, no hooks.
- `~/.claude/skills` holds 20 skills (design, copywriting, SEO, Manim, Remotion and similar), none about version control.
- The outer session’s `CLAUDE_CONFIG_DIR` pointed at a session directory that is not signed in, so it was **unset** for the harness; the agents used the default config and keychain login.
- Agents also ran on the machine’s real home: `AS-CACHE-01` started its node daemon under the real `~/.bee`, and `AS-CORE-01`-style runs could reach any binary on PATH (`python3` 3.11, `git`, `gh`).

Contamination risk is low for the version-control behaviour under test (no instructions, no relevant
skills), but it is not zero, and a result here should not be called isolated.

### What “runs” means: harness fixes that discarded earlier runs

The scenarios had never run against a server before this session, so the first attempts mostly
exposed harness bugs. Runs made before a fix that affected them are discarded, not counted. Kept:
the runs made with the final harness for that scenario. Scenarios `AS-CORE-01`, `AS-CORE-03`,
`AS-CORE-04`, `AS-MERGE-02` and `AS-WC-02` kept their first run, which was made before the
org/repo-access fix below; their first run was therefore on an older fixture than their second and
third.

Discarded: two partial passes aborted on setup errors, 29 runs of the first full pass, and 8 runs
of the second batch that were rerun after a later fix. Fixes, all in `harness/agents`, each with a self-test:

1. Client-streaming requests carried an end-stream frame, which Connect puts only on responses and the server rejects (`INVALID_ARGUMENT`). Every scenario that uploads objects died in setup.
2. `AS-CORE-01` and `AS-GIT-01` re-put a README with the exact bytes `autoInit` already wrote; the server refuses a put that changes nothing.
3. Token scopes listed the repo by **id**; the core gate matches full names, so every ref and change-request write was denied. Scopes now use full names. This is also a product inconsistency (below).
4. Scenarios asked for an action named `admin` that does not exist, and agents lacked `manage_refs`, `triage`, `issues_write`, `purge`; the action lists now use real names.
5. Principals the scenarios create (the worker and every other agent) were not in the org and not repo collaborators, so forge calls answered `NOT_FOUND` and, with a read role, `PERMISSION_DENIED`. Each now joins the org and gets the write role, as `crates/bee-conformance/tests/repo_scoped_tokens.rs` does. This was the largest single cause: the first full pass had 10 setup errors and many agents unable to open a change request.
6. A branch created in setup by an agent was refused (agents hold no `manage_refs` role); the harness admin now points it.
7. `AS-AUTH-01` had an agent create an agent, which the server refuses; the admin creates it.
8. A `unix:` endpoint (the node daemon’s) was fetched as a host named `unix`; the client now dials the socket. `AS-CACHE-01` passes with this.
9. `prefix_of` now accepts the MCP surface’s base64 id as well as the CLI’s `c1_<hex>` text for the same id.
10. proto3 JSON omits zero numbers and empty lists; an absent number now compares as 0 and an absent list has length 0 (`failClosed lte 0`, `alerts len 0`).
11. Scenario setup fixes: `AS-SEC-01` enables security updates, which starts the run (it triggered a second duplicate run before); `AS-ISSUE-01` uses a token not restricted to repos, which project changes need; `AS-STORE-01` makes the worker an org admin with `purge`; a 1,000-blob seed gets a longer upload timeout.

The scenario count: `harness/agents/test/scenarios.test.ts` derives the count from PRODUCT instead of hard-coding 38 (cherry-picked from the CI task’s commit so the harness could run `AS-CR-04`; the same change merges cleanly).

## Results

Columns: runs passed / runs recorded, median wall time and median agent turns across recorded runs
(setup errors have no agent and no time). “blocked” means the harness refused to start the
scenario because a host requirement is missing. The Codex column is pending for every scenario.

| Scenario | Area | Title | Claude Code (sonnet-5-5) | Median time | Median turns | Codex |
|---|---|---|---|---|---|---|
| AS-AGENT-01 | 4.19 | Two agents take overlapping tasks; they discover each other's leases and route around them | 3 / 3 | 110 s | 20 | pending (quota until 2026-11-17) |
| AS-AGENT-02 | 4.19 | A component watcher detects an injected latency regression and opens an issue with evidence | blocked (0 runs) | – | – | pending (quota until 2026-11-17) |
| AS-API-01 | 4.8 | Point unmodified gh at Beehive and open, review and merge a PR | 0 / 1 | 16 s | 3 | pending (quota until 2026-11-17) |
| AS-API-02 | 4.8 | Subscribe to events and react to a failing check within one second of it failing | 1 / 3 | 170 s | 14 | pending (quota until 2026-11-17) |
| AS-AUDIT-01 | 4.10 | Answer "who changed the land rules for main this month, and on whose behalf" from the audit log alone | 0 / 1 | – | – | pending (quota until 2026-11-17) |
| AS-AUTH-01 | 4.9 | Mint a token for a sub-agent limited to docs/ with a budget of five changesets; the sixth commit and writes outside docs/ are refused | 0 / 1 | 29 s | 10 | pending (quota until 2026-11-17) |
| AS-AUTH-02 | 4.9 | Change a ruleset in evaluate mode and report which recent lands it would have blocked | 0 / 1 | 22 s | 10 | pending (quota until 2026-11-17) |
| AS-CACHE-01 | 4.5 | Start 50 jobs on one machine that each check out the same 1,000-file scope; the daemon fetches each object from upstream once | 3 / 3 | 107 s | 9 | pending (quota until 2026-11-17) |
| AS-CI-01 | 4.14 | Add a workflow, watch it fail, read the logs and annotations, fix it, see it pass | 0 / 1 | 887 s | 31 | pending (quota until 2026-11-17) |
| AS-CI-02 | 4.14 | Introduce a regression; observe auto-bisect identify it and open a revert | 0 / 1 | 1681 s | 3 | pending (quota until 2026-11-17) |
| AS-CLI-01 | 4.7 | Using only --help, find, review and land an open change request | 3 / 3 | 43 s | 14 | pending (quota until 2026-11-17) |
| AS-COLLAB-01 | 4.16 | Cut a release with generated notes from the CRs since the last tag | 0 / 1 | 23 s | 7 | pending (quota until 2026-11-17) |
| AS-COLLAB-02 | 4.16 | Work through the notification inbox: review requests, mentions, done | 1 / 3 | 135 s | 47 | pending (quota until 2026-11-17) |
| AS-CORE-01 | 4.1 | Commit and land a one-file fix through the API alone, with no checkout | 3 / 3 | 51 s | 12 | pending (quota until 2026-11-17) |
| AS-CORE-02 | 4.1 | Find which change introduced a given line and who (or which agent) made it | 3 / 3 | 19 s | 6 | pending (quota until 2026-11-17) |
| AS-CORE-03 | 4.1 | Land a three-change stack, then amend the middle change and re-land it | 3 / 3 | 160 s | 30 | pending (quota until 2026-11-17) |
| AS-CORE-04 | 4.1 | Answer "what changed in src/auth since yesterday" without fetching file contents | 3 / 3 | 10 s | 4 | pending (quota until 2026-11-17) |
| AS-CR-01 | 4.13 | Open a CR, get it reviewed by another agent, address suggestions, land via auto-merge | 0 / 1 | 972 s | 40 | pending (quota until 2026-11-17) |
| AS-CR-02 | 4.13 | Land a docs-only change without a human approval and be refused when it touches a sensitive path | 3 / 3 | 56 s | 21 | pending (quota until 2026-11-17) |
| AS-CR-03 | 4.13 | Work a five-CR stack through review and the merge queue | 0 / 1 | – | – | pending (quota until 2026-11-17) |
| AS-CR-04 | 4.13 | Attach a brief and before/after evidence to a UI change, then see the evidence go stale on a revision | 0 / 1 | 90 s | 34 | pending (quota until 2026-11-17) |
| AS-GIT-01 | 4.11 | Using plain git, clone, branch, commit and push a change that lands under policy | 0 / 1 | 20 s | 3 | pending (quota until 2026-11-17) |
| AS-ISSUE-01 | 4.15 | Triage ten new issues into types, labels, sub-issues and a project iteration | 3 / 3 | 125 s | 47 | pending (quota until 2026-11-17) |
| AS-ISSUE-02 | 4.15 | Be assigned an issue, implement it, and close it through a landed CR | 3 / 3 | 61 s | 20 | pending (quota until 2026-11-17) |
| AS-MERGE-01 | 4.2 | Rebase a change onto a moved main, resolve the structured conflict, and land | 2 / 3 | 48 s | 17 | pending (quota until 2026-11-17) |
| AS-MERGE-02 | 4.2 | Land a change across a rename made by another agent without a conflict | 3 / 3 | 40 s | 11 | pending (quota until 2026-11-17) |
| AS-MERGE-03 | 4.2 | Act as a registered resolver: receive a conflict, submit a resolution, see it land | 3 / 3 | 75 s | 20 | pending (quota until 2026-11-17) |
| AS-MIG-01 | 4.12 | Migrate a GitHub repo, verify the fidelity report, and land a change in bridge mode that appears as a PR on GitHub | blocked (0 runs) | – | – | pending (quota until 2026-11-17) |
| AS-MOD-01 | 4.21 | As a moderator agent, hide spam, block its author and set interaction limits | 0 / 1 | 43 s | 14 | pending (quota until 2026-11-17) |
| AS-OPS-01 | 4.22 | As an operator agent, diagnose an injected fault from metrics, logs and traces and name the failing component | blocked (0 runs) | – | – | pending (quota until 2026-11-17) |
| AS-ORG-01 | 4.20 | Provision a team through SCIM, grant it a custom role on three repos, verify access | blocked (0 runs) | – | – | pending (quota until 2026-11-17) |
| AS-PKG-01 | 4.17 | Publish a container image and an npm package from a workflow, then consume both | 0 / 1 | 428 s | 58 | pending (quota until 2026-11-17) |
| AS-SEARCH-01 | 4.4 | Find every call site of a function across an org and open one change fixing them | 3 / 3 | 72 s | 31 | pending (quota until 2026-11-17) |
| AS-SEARCH-02 | 4.4 | Find all open change requests touching billing/ that are waiting on review | 3 / 3 | 15 s | 6 | pending (quota until 2026-11-17) |
| AS-SEC-01 | 4.18 | Respond to a vulnerability alert by landing the update CR and closing the alert | 2 / 3 | 53 s | 17 | pending (quota until 2026-11-17) |
| AS-SEC-02 | 4.18 | Attempt to commit a credential and handle the refusal correctly | 0 / 1 | 66 s | 12 | pending (quota until 2026-11-17) |
| AS-STORE-01 | 4.3 | As an admin agent, run fsck and GC in dry-run, then for real, and report results | 0 / 1 | 24 s | 11 | pending (quota until 2026-11-17) |
| AS-WC-01 | 4.6 | Check out only services/api from a 10^6-file repo, change it, commit, land | blocked (0 runs) | – | – | pending (quota until 2026-11-17) |
| AS-WC-02 | 4.6 | Interrupt a rebase midway, recover, finish it | 3 / 3 | 166 s | 31 | pending (quota until 2026-11-17) |

## Failures

Each entry gives the failing goal-state assertion exactly as the harness reported it (the first
failing one of the last failing run), the scenario, a diagnosis, and where the transcript is.
Transcripts are under `harness/agents/runs/` (gitignored) and archived outside the repo at
`~/.terramind/scenario-runs/bee-launch-2026-11-03/`. Diagnosis classes: **product** (the server
or its contract falls short), **agent** (the agent misread or gave up when the product allowed it),
**environment** (the harness host cannot supply what the scenario needs: a runner, TLS, a fixture).

### Product gaps in one list

1. A forge collaborator role does not reach the core policy gate: `ADMIN`/`MAINTAIN` collaborators are denied `manage_policy` and `manage_refs` (AS-AUDIT-01, AS-AUTH-02, AS-COLLAB-01).
2. Token scopes by repo **id** (documented in `TokenScope.repos`) are denied by the core gate, which compares full names; forge RPCs accept both.
3. An agent cannot own a sub-agent, so sub-agent delegation with a budget is unreachable (AS-AUTH-01).
4. The `mergeQueue` ruleset rule is refused (AS-CR-03).
5. An agent-authored change on a ruleset-protected ref always needs a human approval; the ruleset API cannot set `agents_may_land_without_human` (AS-CR-01, AS-GIT-01).
6. Comment moderation is `UNIMPLEMENTED` (AS-MOD-01).
7. Suspected: stale evidence satisfies the brief checklist (AS-CR-04); `RunFsck`/`RunGc` and push-protection refusals are not found in the audit log (AS-STORE-01, AS-SEC-02); a dependency-update run sometimes opens no change request in 60 s (AS-SEC-01).

#### AS-AGENT-02 (environment)

- Failing assertion (verbatim from the harness report): `blocked`: env BEE_HARNESS_LATENCY_INJECT not set; env BEE_HARNESS_LATENCY_CLEAR not set
- Diagnosis: Needs `BEE_HARNESS_LATENCY_INJECT` and `BEE_HARNESS_LATENCY_CLEAR`, host commands that make a service on the dev server slow. Nothing in the repository provides them, so the harness reports the scenario `blocked` and no agent ran.
- Transcript: none (no agent ran)

#### AS-API-01 (environment)

- Failing assertion (verbatim from the harness report): `fail`: the PR exists, landed, and was opened by the worker: FAIL changes len: no length: undefined; FAIL changes[0].title eq: want "Bump version to 1.1.0", got undefined; FAIL changes[0].author.id eq: want "01M6HK5T6B5NCD2YF7JS8KZFW8", got undefined
- Diagnosis: The agent ran `gh pr create` and got `server gave HTTP response to HTTPS client`: `gh` only talks HTTPS to a host other than github.com, and the harness serves the dev server over plain HTTP. The agent stopped after 3 turns and did not look for a workaround, which is correct. Passing this needs a TLS front for the server (and the agent trusting its certificate), which the harness does not provide.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-API-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-API-02 (agent)

- Failing assertion (verbatim from the harness report): `fail`: the worker commented about the failed check within one second: FAIL comments some: no element matched (createdAt within: "2026-11-03T21:44:32.923+00:00" not within 1000 ms after 2026-11-03T21:35:11.353+00:00)
- Diagnosis: Two of three runs failed the one-second reaction bar, and both agents said why in their final message. Run 1: the watcher exited about 10 s after READY when the server closed the event stream with a deadline, and the agent noticed only later (comment posted about two minutes after the failure). Run 2: the watcher matched `conclusion: FAILURE` but this server reports `status: CHECK_STATUS_FAILED`, so it ignored the event and commented about nine minutes late. The server delivered the events in time in both runs. Agent confusion about a stream that ends and about the check status field, not a product gap.
- Transcript: `harness/agents/runs/pass2/20261103T213114Z/AS-API-02/claude-code/2/` (agent.stdout.jsonl, result.json)

#### AS-AUDIT-01 (product)

- Failing assertion (verbatim from the harness report): `setup-error`: the policy bot changes the main land rules: FAIL PolicyService/UpdateRuleset: POLICY_DENIED: 01M6HK5T6G3ACDVB1B3A33CDZ8 holds no role granting `manage_policy`
- Diagnosis: Setup, not the agent, failed. The policy bot is a collaborator with `REPO_ROLE_ADMIN` and a token carrying `manage_policy`, yet `UpdateRuleset` is denied: the core gate requires every principal in the delegation chain to hold `manage_policy` through the policy document’s `[members]` roles, and a forge collaborator role does not put the agent there. So the "bot changed the rules on the admin’s behalf" history the scenario asks an agent to read can never be created through the API.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-AUDIT-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-AUTH-01 (product)

- Failing assertion (verbatim from the harness report): `fail`: the token acts as the sub-agent with a five-changeset budget, writing only under docs/: FAIL agent did not write subagent.token
- Diagnosis: The agent did the right thing and reported the failure: `MintToken` for the sub-agent returned `PERMISSION_DENIED: tokens can be minted only for yourself or for agents you own`. An agent cannot create an agent (`CreateAgent` is refused for agent callers) and `CreateAgentRequest` has no owner field, so no principal an agent owns can exist; the harness admin creates the sub-agent in setup and the worker does not own it. Delegation to a sub-agent with a budget (PRODUCT §4.9) is not reachable by an agent today.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-AUTH-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-AUTH-02 (product)

- Failing assertion (verbatim from the harness report): `fail`: the ruleset is in evaluate mode and restricts infra/: FAIL enforcement eq: want "RULESET_ENFORCEMENT_EVALUATE", got "RULESET_ENFORCEMENT_DISABLED"; FAIL rules some: no element matched (restrictedPaths.pathGlobs truthy: got undefined); FAIL rules[*].restrictedPaths.pathGlobs[*] matches: [] !~ /^/?infra(/|$)/
- Diagnosis: The agent read the ruleset, tried to switch it to evaluate mode and simulate it, and was denied both times: `holds no role granting manage_policy`. Same cause as AS-AUDIT-01: the worker is a repo `ADMIN` collaborator with a `manage_policy` token, and the core gate does not count the collaborator role. The agent wrote no answer and did not try to get around the denial.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-AUTH-02/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-CI-01 (environment)

- Failing assertion (verbatim from the harness report): `fail`: at least one run failed: FAIL runs len_gte: no length: undefined
- Diagnosis: The agent added `.github/workflows/ci.yml`, landed it, and the run sat in `QUEUED` for the rest of its 887 s: the dev server has no runner attached, so no workflow job can start. The agent diagnosed this correctly. The harness has no way to attach a runner (PRODUCT §4.14 expects the stock `actions/runner` or a hosted one).
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-CI-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-CI-02 (environment)

- Failing assertion (verbatim from the harness report): `fail`: a bisect found it: FAIL bisects some: not an array: undefined
- Diagnosis: Same cause as AS-CI-01: no runner, so the check suites on main never left `QUEUED`, no failure was ever detected and no bisect ran. The agent waited about 28 minutes and stopped without reverting by hand.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-CI-02/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-COLLAB-01 (product)

- Failing assertion (verbatim from the harness report): `fail`: v1.1.0 is published at the tip of main with generated notes listing exactly the new CRs: FAIL ReleaseService/GetReleaseByTag: NOT_FOUND: release for tag v1.1.0 not found
- Diagnosis: The agent generated the release notes correctly and then `CreateRelease` was denied: `holds no role granting manage_refs`. The worker is a repo `MAINTAIN` collaborator and its token lists `manage_refs`, but the collaborator role does not reach the core policy gate (as in AS-AUDIT-01). Tag and release creation by an agent with a forge maintain role is therefore refused.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-COLLAB-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-COLLAB-02 (agent)

- Failing assertion (verbatim from the harness report): `fail`: the worker reviewed the CR: FAIL reviews some: not an array: undefined
- Diagnosis: Passed once, failed twice. In both failures the agent answered the issue question but could not submit its review or mark the inbox threads done, so the final inbox was not empty. The transcripts show it passing `repo` as a bare string ("org/app") where the schema wants a `RepoRef` object, getting `invalid type: string … expected struct bee.v1.RepoRef`, and then not recovering for the review and thread calls. The error text is precise, so this is agent confusion; the MCP argument shape is the thing to watch.
- Transcript: `harness/agents/runs/pass2/20261103T213114Z/AS-COLLAB-02/claude-code/2/` (agent.stdout.jsonl, result.json)

#### AS-CR-01 (product)

- Failing assertion (verbatim from the harness report): `fail`: the worker's CR landed: FAIL changes[0].state eq: want "CHANGE_STATE_LANDED", got "CHANGE_STATE_OPEN"
- Diagnosis: The reviewer approved revision 2 and the CR stayed open: `land[main] policy also requires a human approval`. The built-in rule in `bee-policy/src/land.rs` makes an agent-authored change in any diff class outside `agents_may_land_without_human` wait for a human approval, and the ruleset API has no field to set that list (it lives in `.beehive/policy.toml`). The scenario says agent approvals count (`agentsMayApprove: true`) and cannot be satisfied by two agents. The agent explained the block accurately.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-CR-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-CR-03 (product)

- Failing assertion (verbatim from the harness report): `setup-error`: merge queue and one approval on main: FAIL PolicyService/CreateRuleset: INVALID_ARGUMENT: merge_queue has no land-policy equivalent on this server (rulesets are enforced through SPEC §9.4 policy)
- Diagnosis: Setup, not the agent, failed: `CreateRuleset` rejects the `mergeQueue` rule with `merge_queue has no land-policy equivalent on this server`. PRODUCT §4.13 lists the merge queue as a ruleset feature and this scenario is built on it; the server refuses to store such a ruleset, so a repo cannot require the queue.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-CR-03/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-CR-04 (product)

- Failing assertion (verbatim from the harness report): `fail`: the checklist still owes visual evidence for the current revision: FAIL items some: no element matched (id eq: want "evidence:ui", got "brief")
- Diagnosis: The agent opened the CR with two revisions, wrote the brief and attached before/after evidence; the evidence is reported stale after the second revision as expected, but `GetBriefChecklist` then lists only the `brief` item as satisfied and no longer owes `evidence:ui`. Either stale evidence still satisfies the checklist, or the agent attached fresh evidence the assertion cannot tell apart; the transcript does not settle which, so this is a suspected product gap in the checklist rather than a confirmed one.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-CR-04/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-GIT-01 (product)

- Failing assertion (verbatim from the harness report): `fail`: main has CONTRIBUTING.md: FAIL QueryService/ReadPath: NOT_FOUND: path "CONTRIBUTING.md" not found
- Diagnosis: The agent cloned over plain git, committed `CONTRIBUTING.md` on a branch and pushed to main; the push was refused with `land refused by policy; missing: approval:human`. Same built-in human-approval rule as AS-CR-01: a ruleset on main makes an agent’s push wait for a human, so "get that commit onto main by pushing it" cannot succeed for an agent on a repo with a ruleset. The agent stopped and did not force-push or work around it.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-GIT-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-MERGE-01 (agent)

- Failing assertion (verbatim from the harness report): `fail`: the landed tip is the same piece of work (rebased, same ChangeId): FAIL changesets[0].changeId eq: want {"raw":"Ti6kMlro1i7yn6X7WSTguQ=="}, got {"raw":"EW4a7TbE0OdGrbC6ZH2DYg=="}
- Diagnosis: Two of three runs passed. In the failing run the rebase hit a real content conflict on the `DISCOUNT` line; the agent resolved it by hand and landed it as a merge changeset with two parents rather than a rebased changeset keeping the original ChangeId, so the assertion that the tip is "the same piece of work (rebased, same ChangeId)" failed. Agent choice, not a product gap.
- Transcript: `harness/agents/runs/pass2/20261103T213114Z/AS-MERGE-01/claude-code/2/` (agent.stdout.jsonl, result.json)

#### AS-MIG-01 (environment)

- Failing assertion (verbatim from the harness report): `blocked`: env BEE_HARNESS_GITHUB_TOKEN not set; env BEE_HARNESS_GITHUB_REPO not set
- Diagnosis: Needs a real GitHub repository and token (`BEE_HARNESS_GITHUB_TOKEN`, `BEE_HARNESS_GITHUB_REPO`) to migrate from. Not available on this machine, so no agent ran.
- Transcript: none (no agent ran)

#### AS-MOD-01 (product)

- Failing assertion (verbatim from the harness report): `fail`: both spam comments are hidden as spam; the genuine one is visible: FAIL comments count_gte: 0 matching, want >= 2
- Diagnosis: The agent set the interaction limit and then hit `Moderation.MinimizeComment: UNIMPLEMENTED: comments are not available to moderation on this deployment`, so the spam comments stayed visible, and it could not block the spammer either. Hiding comments as spam and blocking a user are the point of PRODUCT §4.17; the server answers UNIMPLEMENTED for comment moderation.
- Transcript: `harness/agents/runs/pass1c/20261103T212755Z/AS-MOD-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-OPS-01 (environment)

- Failing assertion (verbatim from the harness report): `blocked`: env BEE_HARNESS_FAULT_INJECT not set; env BEE_HARNESS_FAULT_CLEAR not set
- Diagnosis: Needs `BEE_HARNESS_FAULT_INJECT` and `BEE_HARNESS_FAULT_CLEAR`, host commands that inject a fault into a component of the dev server. None exist in the repository, so no agent ran.
- Transcript: none (no agent ran)

#### AS-ORG-01 (environment)

- Failing assertion (verbatim from the harness report): `blocked`: fixture enterprise not provided (--fixture enterprise=…)
- Diagnosis: Needs the pre-provisioned `enterprise` fixture (`--fixture enterprise=…`), an enterprise account with SCIM and teams. No generator for it exists, so no agent ran.
- Transcript: none (no agent ran)

#### AS-PKG-01 (environment)

- Failing assertion (verbatim from the harness report): `fail`: the container image was published by a workflow run: FAIL PackageService/ListPackageVersions: NOT_FOUND: package hmw16hfze44fba5/hello not found
- Diagnosis: Same cause as AS-CI-01: the publish workflow runs never started because no runner is attached; the agent landed two probe workflows, saw both runs stay queued and stopped at 87 turns.
- Transcript: `harness/agents/runs/pass1b/20261103T205624Z/AS-PKG-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-SEC-01 (product)

- Failing assertion (verbatim from the harness report): `setup-error`: the alert is open and has an update CR: ok alerts len; FAIL alerts[0].updateChangeRequest truthy: got undefined
- Diagnosis: Two of three runs passed. In the third, setup failed: security updates were enabled but the alert never gained an update change request within the 60 s retry window (`alerts[0].updateChangeRequest truthy: got undefined`), so no agent ran. This looks like a race or a missed trigger in the dependency-update run, not agent behaviour.
- Transcript: `harness/agents/runs/pass2/20261103T213114Z/AS-SEC-01/claude-code/2/` (agent.stdout.jsonl, result.json)

#### AS-SEC-02 (agent)

- Failing assertion (verbatim from the harness report): `fail`: the server refused the credential for the worker at least once: FAIL entries some: no element matched (result eq: want "SECRET_DETECTED", got "ok")
- Diagnosis: The agent wrote the AWS-style keys into `src/aws_config.py` as the prompt asked, was refused with `SECRET_DETECTED`, and stopped because it would not use the bypass. The scenario wanted it to land the change with the client reading credentials from the environment instead, so this is the agent being over-cautious about the goal. A second failed assertion (`AuditService/QueryAuditLog` has no entry with `result: SECRET_DETECTED`) suggests the refused push is not written to the audit log; with the agent having stopped early that check cannot be separated from the agent’s failure, so it is a suspected product gap.
- Transcript: `harness/agents/runs/pass1c/20261103T212755Z/AS-SEC-02/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-STORE-01 (product)

- Failing assertion (verbatim from the harness report): `fail`: fsck, a GC dry run and a real GC ran as the worker, in that order: FAIL entries[*].action matches: [] !~ /fsck/; FAIL entries count_gte: not an array: undefined
- Diagnosis: The agent ran fsck, a GC dry run and a real GC, and wrote `report.json` (after the setup fix that made the worker an org admin with `purge`). It failed on `AuditService/QueryAuditLog`: no audit entry has `action` matching `fsck` for the worker. Either `RunFsck`/`RunGc` are not audited, or they are audited under a different actor than the worker; suspected product gap. The agent also noted the GC response carried no counts (zeros omitted by proto3 JSON), which it reported as 0.
- Transcript: `harness/agents/runs/pass1c/20261103T212755Z/AS-STORE-01/claude-code/1/` (agent.stdout.jsonl, result.json)

#### AS-WC-01 (environment)

- Failing assertion (verbatim from the harness report): `blocked`: fixture mono1m not provided (--fixture mono1m=…)
- Diagnosis: Needs the pre-provisioned `mono1m` fixture (`--fixture mono1m=…`), a 10^6-file repository. Building one at this size is the storage-format work parked in design decision D5 and the CreateChangeset-at-size miss (SPEC §16), so no agent ran.
- Transcript: none (no agent ran)

## What this does and does not show

- 15 of 39 scenarios pass 3/3 on Claude Code, with a small caveat on five of them (above). Passing here is one runtime, one model, non-isolated.
- A scenario that failed once was not rerun unless a harness bug explained it, as the task required, so the single-run failures are one sample each. The agent-classed ones (AS-API-02, AS-COLLAB-02, AS-MERGE-01) show real run-to-run variance.
- The product-classed failures are mostly visible from the first denied call and a transcript; they are not model noise.
- 3 scenarios (AS-CI-01, AS-CI-02, AS-PKG-01) need a workflow runner the harness cannot attach, and 5 need host fixtures that do not exist. Until those exist the §3.1 bar cannot be met for them.
