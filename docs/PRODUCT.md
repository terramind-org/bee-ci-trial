# bee + Beehive: product spec, scope and definition of done

Status: **draft for approval, 2026-10-17.** This document decides *what* we build and *how we
know it is done*. `SPEC.md` and the documents it points to decide *how* it works. If the two
disagree on scope, this document wins and the spec gets amended.

## 1. The product in one paragraph

**bee** is source control built for AI agents. History, merges, rebases and landing run on
the server; clients fetch only the objects they need, or work purely through the API.
**Beehive** is a complete forge on top of it: everything GitHub does, rebuilt so that thousands
of agents can work in one repository at once without colliding, without waiting, and with a
record of who did what on whose behalf. One server binary, self-hostable, open source.

## 2. Scope rule

**Everything is in scope.** Every capability in Anthropic's *Redesigning source control*, every
contract and feature in the Scion reference (including the ones Scion itself deferred), and every
GitHub.com feature as of October 2026. Nothing is marked "v1", "later" or "non-goal". The build
order in §6 is a sequence, not a cut: the product is done only when every area in §4 meets its
done-when.

Two boundaries are design decisions, not scope cuts:

- **Clients are online.** The native protocol assumes a reachable server, as the post does.
  Offline work is still possible through the git gateway (§4.11): any git client works against
  Beehive, offline included.
- **We don't ship an IDE or code completion.** Copilot-style completions belong to editors.
  Beehive provides the APIs, the agent runner and a model gateway (§4.20) that editors and
  agents use.

Traceability: every item from the three source audits is a row in
`docs/coverage/features.csv`, mapped to an area below and to the test that proves it.
`scripts/check-coverage.py` fails if any row lacks an area, an acceptance test, or a source.
Sources are in `docs/research/`.

## 3. What "done" means everywhere

These rules apply to every area. An area is done only when all of them hold.

### 3.1 Agents can use it: the pass/fail bar

A feature that agents cannot use is not done, however good the code is.

- Every area defines **agent scenarios** (`AS-…` below). A scenario is a task prompt plus a
  goal state checked by API assertions, never by reading the agent's transcript.
- A scenario is run by a **fresh agent** that gets only the task prompt and the product's own
  surfaces: `bee --help`, the MCP tool list, and the API reference. No hints, no hand-written
  examples beyond what ships with the product.
- **Pass:** the goal state holds in **3 out of 3 runs on each of at least two agent runtimes**
  (for example Claude Code and Codex), with no human intervention.
- The scenario harness, its prompts and its assertions live in the repository and run in CI
  against a fresh `bee-server --dev`. Results are published per release.

### 3.2 APIs and CLI are contracts, and are tested as contracts

- Every operation exists in **three forms that behave identically**: the native API (gRPC and
  HTTP/JSON from one protobuf schema), the CLI with `--json`, and an MCP tool. A conformance test
  calls all three for every operation and compares results.
- Every CLI command's JSON output validates against a **published, versioned JSON Schema**. Every
  error carries a **stable reason code** from one table, a `retryable` flag and a typed payload.
  Exit codes are fixed per outcome.
- **API conformance suite:** a standalone runner (`bee conformance run --endpoint <url>`)
  exercises every RPC against any server, including third-party implementations, and reports
  per-RPC pass/fail.
- **Compatibility suites:** the GitHub REST/GraphQL compatibility layer runs the relevant parts of
  `gh`, Octokit and `actions/runner` against Beehive; the git gateway runs a git client matrix.
- Everything is **testable locally** with one command and no external services:
  `bee-server --dev` plus `scripts/check.sh`. External backends (Postgres, S3) are tested when
  their environment variables are set.

### 3.3 Everything is auditable

- An **append-only, hash-chained audit log** records every state-changing action: who (full
  delegation chain), what, when, from where, and the policy decision that allowed it. Agents and
  admins query it through the API and CLI; it streams to external sinks.
- Every changeset carries **provenance** (agent, model, session, checks run), and every ref
  update carries a ref-log entry. "Why is this line here" is answerable from the API alone.
- **Coverage is auditable:** `docs/coverage/features.csv` lists every feature with its source, its
  area and its test; CI publishes which tests pass.

### 3.4 Correctness, performance and reliability gates

- **Correctness:** independent reference model, golden vectors, fuzzing of every decoder and
  parser, fault injection at every storage step, linearizability checks, differential tests against
  git. Details in §4.25.
- **Performance:** the targets in §4.23 are measured by the load harness on every release; a
  missed target blocks release unless explicitly waived in the release notes.
- **Reliability:** the failure-isolation properties in §4.22 are each covered by a fault test
  that proves the property (for example: stop the forge database, `git clone` still works).

---

## 4. Scope by area

Each area lists what it includes, what done looks like, and its agent scenarios. Capabilities are
written as outcomes; mechanisms live in the technical spec.

### 4.1 Core version control

**Includes.**
- Content-addressed objects (blob, tree, changeset, meta) with canonical encoding and typed ids.
- Changesets that record their delta from the first parent; immutable, verifiable.
- Stable ChangeIds that survive amend and rebase.
- Server-side commit (CreateChangeset), rebase, merge and land; idempotent retries.
- Lock-free landing; versioned refs with a full ref log; ref name grammar; fast-forward rules
  and force permission.
- Revision expressions (`main~3`, `ref@version`, `git:<sha>`, change ids, unique prefixes).
- History without a clone: log, path history, blame, diff, ancestry, merge base, read path,
  list tree.
- Very large repositories: sharded trees for huge directories, chunked large blobs with
  verified range reads, no size cliffs.
- Line-ending and attribute rules (`.gitattributes` semantics).

**Done when.**
- Every operation above is available through API, CLI and MCP and passes the conformance suite.
- Committing one file costs O(depth) storage reads at 10³, 10⁵ and 10⁷ files (complexity gate).
- Retrying any write after "committed, response lost" returns the original result.
- The reference model agrees with production on 10⁶ randomized operations.

**Agent scenarios.**
- `AS-CORE-01` Commit and land a one-file fix through the API alone, with no checkout.
- `AS-CORE-02` Find which change introduced a given line and who (or which agent) made it.
- `AS-CORE-03` Land a three-change stack, then amend the middle change and re-land it.
- `AS-CORE-04` Answer "what changed in `src/auth` since yesterday" without fetching file contents.

### 4.2 Merging and conflicts

**Includes.**
- Deterministic three-way line merge with a frozen, published algorithm.
- Rename- and copy-aware merging.
- Criss-cross merges (several best merge bases) with a virtual base.
- Two-way merge of independent adds.
- Structured conflicts as data (never markers on the server); exact marker format for working
  copies.
- A resolver hook that hands conflicts to an agent and lands its resolution under policy.
- A web conflict editor.

**Done when.**
- The merge contract has worked examples and golden vectors; differential tests against
  `git merge-file` pass with every difference documented.
- Concurrent agents editing the same files in a simulation land without lost updates, and every
  conflict is either auto-resolved by the resolver hook or surfaced with a structured payload.

**Agent scenarios.**
- `AS-MERGE-01` Rebase a change onto a moved `main`, resolve the structured conflict, and land.
- `AS-MERGE-02` Land a change across a rename made by another agent without a conflict.
- `AS-MERGE-03` Act as a registered resolver: receive a conflict, submit a resolution, see it land.

### 4.3 Storage, durability and lifecycle

**Includes.**
- Metadata store (Postgres in production, SQLite in dev) and blob store (S3-compatible or disk).
- Defined write ordering so no reader sees a reference to bytes that are not durable.
- Object states (live, quarantined, purged); upload records; transaction retry rules.
- Garbage collection of orphans and abandoned drafts with safety windows and dry runs.
- Backups, point-in-time restore and a tested disaster-recovery procedure.
- fsck with an enumerated list of violations.

**Done when.**
- Fault injection at every write-ordering step, including crashes, never leaves a dangling
  reference (fsck clean).
- A restore drill rebuilds a cell from backup and passes fsck and the conformance suite.

**Agent scenarios.**
- `AS-STORE-01` As an admin agent, run fsck and GC in dry-run, then for real, and report results.

### 4.4 Derived data, indexes and search

**Includes.**
- Derived-data framework (eager and lazy, versioned) and a generic GetDerived API.
- Blame, path history across delete and re-add, commit-graph skip pointers.
- Copy detection index.
- Code search: regex, symbols and path/language qualifiers, across repos, honoring path
  permissions.
- Code navigation (definitions and references).
- Language statistics and license detection.
- Query languages for issues and change requests (GitHub-compatible qualifiers), plus repo,
  commit, user, topic, discussion and package search. Saved searches.

**Done when.**
- Search results never include content the caller cannot read (permission matrix test).
- Indexes rebuild from scratch deterministically; a version bump re-derives with no client change.

**Agent scenarios.**
- `AS-SEARCH-01` Find every call site of a function across an org and open one change fixing them.
- `AS-SEARCH-02` Find all open change requests touching `billing/` that are waiting on review.

### 4.5 Fetch and cache tiers

**Includes.**
- Fetch protocol without "have" lists; verified streaming with resume.
- Per-node daemon cache shared by all jobs on a machine.
- Regional cache servers streaming objects from memory.
- Daemon bootstrap from periodically refreshed, verified snapshots.
- One RPC surface served identically by every tier; tiers talk only to the tier above.
- Fail-closed authorization and redaction in caches.

**Done when.**
- Tier conformance: the same read suite returns byte-identical results from server, regional
  cache and daemon.
- Each tier sustains a hit rate above 99% on the reference CI workload, measured by the harness.

**Agent scenarios.**
- `AS-CACHE-01` Start 50 jobs on one machine that each check out the same 1,000-file scope; the
  daemon fetches each object from upstream once.

### 4.6 Working copy and checkout

**Includes.**
- Sparse checkout with add/remove; `goto` that preserves unrelated edits.
- Virtualized checkout that materializes files on demand (Linux and macOS).
- Status from a filesystem watcher, with a stat-cache walk as fallback.
- Commit and amend without a staging area; rebase, resolve and abort.
- `.gitignore` semantics; journaled mutating commands with crash recovery; an advisory lock.
- Case-insensitive filesystem handling, symlink safety, Windows clients.

**Done when.**
- Status agrees with a full rehash under randomized edits, including same-second edits.
- Killing the client at every step of every mutating command leaves a recoverable working copy.

**Agent scenarios.**
- `AS-WC-01` Check out only `services/api` from a 10⁶-file repo, change it, commit, land.
- `AS-WC-02` Interrupt a rebase midway, recover, finish it.

### 4.7 Command-line client

**Includes.** The complete command set:
- **Identity:** `login`, `whoami`, token minting and revocation.
- **Working copy:** `checkout`, `sparse add/remove`, `status`, `diff`, `commit`, `amend`, `goto`,
  `rebase`, `resolve`, `land`.
- **History and content:** `log`, `blame`, `show`, `cat`.
- **Admin:** `redact`, `gc`, `fsck`, `audit`.
- **Integrations:** `import`, `export`, `daemon`, `mcp`, `conformance`, `git` helpers
  (credential, setup, preflight).
- **Forge:** commands for every Beehive object (`cr`, `review`, `issue`, `release`, `lease`,
  `check`, `workflow`, `package`, …).
- **Global flags and output:** `--json`, `--endpoint`, `-C`; fixed exit codes; one JSON envelope.

**Done when.**
- Every command has a JSON Schema, a contract test, and an MCP equivalent.
- `bee <cmd> --help` alone is enough for a fresh agent to use it (verified by the agent
  scenarios across all areas).

**Agent scenarios.**
- `AS-CLI-01` Using only `--help`, find, review and land an open change request.

### 4.8 APIs and integrations

**Includes.**
- Native typed API over Connect (gRPC, gRPC-Web, HTTP/JSON) from one protobuf schema.
- MCP server (stdio and HTTP).
- Event stream with resume.
- Webhooks: repo, org and app level, with delivery logs, redelivery and a GitHub-compatible
  payload format.
- **GitHub REST and GraphQL compatibility layer**, so `gh`, Octokit, GitHub Apps and Actions work
  against Beehive.
- Official SDKs generated from the schema; API versioning and deprecation policy.

**Done when.**
- The GitHub compatibility suite passes the `gh` command matrix (`pr`, `issue`, `run`, `release`,
  `repo`, `api`) and the Octokit endpoints the product claims to support. Those claims are listed
  in a published compatibility table.
- Agents never need to poll: every state they wait for is an event.

**Agent scenarios.**
- `AS-API-01` Point unmodified `gh` at Beehive and open, review and merge a PR.
- `AS-API-02` Subscribe to events and react to a failing check within one second of it failing.

### 4.9 Identity, authentication and policy

**Includes.**
- Principals: humans, agents (always with an owner) and services.
- Delegated, attenuated, budgeted tokens with one-step revocation of a whole delegation tree.
- Signing-key rotation; GitHub/OIDC login, passkeys and 2FA; SSH keys; GPG and SSH signing keys.
- OAuth provider and app installations.
- Policy as code: path and ref permissions, roles, custom roles, rulesets with bypass lists and
  evaluate mode, push rules (paths, extensions, sizes).
- Required signed commits; owners/CODEOWNERS; an authorization table for every RPC; per-repo
  policy epochs.

**Done when.**
- An access-matrix test covers every RPC × principal kind × path class, through server and
  caches, and proves no content leaks through existence probes, search, diffs or caches.
- Revoking a root token stops every derived token within the propagation bound.

**Agent scenarios.**
- `AS-AUTH-01` Mint a token for a sub-agent limited to `docs/`, with a budget of five changesets;
  the sub-agent's sixth commit and any write outside `docs/` are refused with the right reasons.
- `AS-AUTH-02` Change a ruleset in evaluate mode and report which recent lands it would have blocked.

### 4.10 Redaction, compliance and audit

**Includes.**
- Redaction lifecycle (live → quarantined → purged), unredact from quarantine, scheduled and
  immediate purge, propagation bound, placeholders in working copies.
- Content takedown process for repos and comments.
- Org and enterprise audit log, hash-chained, streamable to SIEM sinks.

**Done when.**
- A redacted object is unreadable through every tier within the bound; changeset ids never change.
- Audit log hash chain verifies end to end; every state-changing RPC writes an entry.

**Agent scenarios.**
- `AS-AUDIT-01` Answer "who changed the land rules for `main` this month, and on whose behalf"
  from the audit log alone.

### 4.11 Git compatibility

**Includes.**
- Git gateway over smart HTTP and SSH, protocol v2, shallow and partial clone, seeded packs.
- Push to protected branches goes through Land under policy; workspace pushes; change-id trailers.
- Exact git commit ids for imported history and deterministic git ids for new history.
- LFS served natively; tags (lightweight and annotated); submodules; signed commits preserved.
- A git client matrix test.

**Done when.**
- Clone, fetch and push work from every client in the matrix (git versions, GitHub Desktop, common
  IDEs).
- Import followed by export reproduces every git object byte-for-byte.

**Agent scenarios.**
- `AS-GIT-01` Using plain `git`, clone, branch, commit and push a change that lands under policy.

### 4.12 Migration

**Includes.**
- One-command import from GitHub (repo or org) with dry run, git-first availability, metadata
  backfill, live sync and a fidelity report.
- Three reversible phases: mirror, bridge, cutover.
- Claimable identities; preserved PR and issue numbers; link redirects.
- Importers for GitLab, Bitbucket, Azure DevOps and Gitea.
- Export to GitHub's migration-archive format at any time.

**Done when.**
- Importing a large public repository reports zero unexplained drops in the fidelity report.
  The reference repository is chosen in the test plan, at Kubernetes scale or the largest
  available.
- A repo can go GitHub → Beehive → GitHub with no loss beyond what the fidelity report lists.

**Agent scenarios.**
- `AS-MIG-01` Migrate a GitHub repo, verify the fidelity report, and land a change in bridge mode
  that appears as a PR on GitHub.

### 4.13 Change requests and review

**Includes.** Everything GitHub pull requests do, plus agent-native review:
- **Lifecycle:** drafts, reopen and edit, requested reviewers and auto-assignment.
- **Comments and threads:** line, multi-line, file-level and conversation comments; threads with
  resolution; suggestions with batch apply; reactions; "viewed" file state.
- **Review rules:** stale-approval rules, last-push approval, CODEOWNERS.
- **Links and templates:** linked issues with closing keywords, templates, labels and milestones.
- **Views:** compare view, diff options, commit comments, revert button, conversation locking.
- **Merging:** merge methods (rebase, squash, merge commit), auto-merge, the merge queue with
  landing trains, stacked change requests.
- **Agent-native:** semantic diff classes driving approvals, revisions with "changes since my
  review", agent reviewers, summaries.
- **Briefs and evidence** (SPEC §11.9): a change request reads as a brief (outcome, highlights,
  asks, risks, rollback) with media evidence (screenshots, before/after, video, benchmarks, test
  reports, preview links) labelled by source and flagged stale after a new revision, and an
  advisory checklist of what the author still owes. It nudges and never gates landing.

**Done when.**
- Every GitHub PR feature in the coverage matrix has a passing test.
- Under 1,000 concurrent landers on one ref, the merge queue meets its fairness and throughput
  targets (§4.23).

**Agent scenarios.**
- `AS-CR-01` Open a CR, get it reviewed by another agent, address suggestions, land via auto-merge.
- `AS-CR-02` Land a docs-only change without a human approval and be refused when it touches a
  sensitive path.
- `AS-CR-03` Work a five-CR stack through review and the merge queue.
- `AS-CR-04` As an agent, open a UI change, read its checklist, attach a brief and before/after
  evidence with the CLI and watch the change page's Brief tab fill in live; revise the change and
  see the earlier screenshot flagged stale while landing stays allowed.

### 4.14 Checks, CI and workflows

**Includes.**
- **Checks:** check runs and suites with annotations, re-run and output; commit statuses. Check
  output, CI-attached evidence and run outcomes read in plain words.
- **Workflow engine:** runs `.github/workflows` YAML compatibly, covering triggers, matrix,
  concurrency, reusable workflows, composite, JavaScript and Docker actions, service containers.
- **Runners:** hosted (Linux, macOS, Windows, ARM, GPU) and self-hosted using the stock runner
  protocol.
- **Build services:** caches, artifacts, logs, job summaries; secrets and variables at repo, env
  and org level; scoped job tokens; OIDC to clouds; artifact attestations.
- **Deployments and environments** with protection rules.
- **Post-land testing** with automatic bisect and revert; `green/*` refs gating deploys.
- **Actions policies** and required workflows.

**Done when.**
- A published corpus of real-world workflows (from popular open-source repos) runs unmodified with
  the same outcomes as on GitHub, beyond documented differences.
- A breaking change landed on `main` is bisected and reverted automatically within the target time.

**Agent scenarios.**
- `AS-CI-01` Add a workflow, watch it fail, read the logs and annotations, fix it, see it pass.
- `AS-CI-02` Introduce a regression; observe auto-bisect identify it and open a revert.

### 4.15 Issues and planning

**Includes.**
- **Issues:** types, sub-issues, dependencies, custom fields, templates and forms, labels,
  milestones, assignees (including agents), pinning, locking, transfer, close reasons,
  duplicates, task lists, mentions and cross-references, saved replies.
- **Projects:** table, board and roadmap views, fields, iterations, automations, insights,
  status updates, templates, public projects.

**Done when.**
- Assigning an issue to an agent starts that agent's work and links the resulting CR, end to end.
- Projects are fully driven through API and MCP (no UI-only operations).

**Agent scenarios.**
- `AS-ISSUE-01` Triage ten new issues into types, labels, sub-issues and a project iteration.
- `AS-ISSUE-02` Be assigned an issue, implement it, and close it through a landed CR.

### 4.16 Collaboration surfaces

**Includes.**
- Discussions with categories, answers, polls and announcements.
- Wikis as cloneable sibling repos; gists.
- Releases with assets, generated notes and immutable releases; source archives.
- Notifications: inbox, watching, subscriptions, email, reply-by-email, scheduled reminders.
- Profiles with READMEs, contribution graphs, followers, stars and lists, explore, trending and
  topics.
- Sponsors and FUNDING.yml.
- Repository insights: pulse, contributors, traffic, network graph, dependency insights.
- Templates, transfer, archiving, topics, social previews, community files and citation files.

**Done when.** Every row in the coverage matrix for these features has a passing API test; the
notification inbox is reachable and actionable through MCP.

**Agent scenarios.**
- `AS-COLLAB-01` Cut a release with generated notes from the CRs since the last tag.
- `AS-COLLAB-02` Work through the notification inbox: review requests, mentions, done.

### 4.17 Packages and Pages

**Includes.**
- Registries for OCI containers, npm, Maven/Gradle, NuGet and RubyGems with one package model,
  visibility, permissions and provenance.
- Static site hosting from a ref, a folder or a workflow artifact, with custom domains, automatic
  certificates and access-controlled sites.

**Done when.** Each registry passes its client's standard publish and install flow (docker,
npm, mvn, dotnet, gem); a site deploys from a ref and serves over HTTPS on a custom domain.

**Agent scenarios.**
- `AS-PKG-01` Publish a container image and an npm package from a workflow, then consume both.

### 4.18 Security

**Includes.**
- Dependency graph and SBOM export (SPDX, CycloneDX).
- Vulnerability alerts from an advisory database; automated security and version updates.
- Dependency review on CRs.
- Code scanning with SARIF ingest and alerts.
- Secret scanning on ingest with **push protection** at commit and land.
- Repository security advisories; private vulnerability reporting.
- Security overview and campaigns; code quality findings.
- Signature verification with provenance shown alongside.

**Done when.** A known-vulnerable dependency raises an alert and an update CR; committing a test
secret is refused at CreateChangeset, at land and at git push, with the bypass audited.

**Agent scenarios.**
- `AS-SEC-01` Respond to a vulnerability alert by landing the update CR and closing the alert.
- `AS-SEC-02` Attempt to commit a credential and handle the refusal correctly.

### 4.19 Agents and workspaces

**Includes.**
- Agent principals with owners, runtimes and purposes; per-agent dashboards (work, cost, outcomes).
- A native agent runner: assign an issue or a review to an agent and it works in a sandbox, opens
  a CR, iterates on review.
- An agent registry for any runtime (Claude Code, Codex, others).
- Path leases; provenance; a repo context API; agent instructions read from the repo.
- Agent memory and context bundles.
- Swarms for agents working together in one repository, with one lead and one accountable
  person, task claims and hand-backs, policy-checked joins, and outcome links to pull requests.
  The repository work list includes independent runs.
- Safety records compare requested work with stored evidence, accept independent review,
  and bring tripwires and agent-raised concerns to the person who answers for the work.
- Workspaces: devcontainer-based cloud environments for humans and agents, with prebuilds,
  secrets, ports and dotfiles, and sandbox egress policy.
- **Component watchers ("superfans"):** agents scoped to one component with access to its
  telemetry, logs and an anomaly feed.

**Done when.**
- Two agents given overlapping tasks in one repo coordinate through leases and both land without a
  conflict, in the simulation and in a live scenario.
- Every agent action is attributable through provenance and the audit log to a human or service
  root.
- A person can steer a swarm through its lead, review task attempts and hand-backs, and see the
  pull requests produced by both swarms and independent runs in one repository work list.
- A person can review a sampled agent merge and can stop agent work and hold agent merges
  across their organization.

**Agent scenarios.**
- `AS-AGENT-01` Two agents take overlapping tasks; they discover each other's leases and route
  around them.
- `AS-AGENT-02` A component watcher detects an injected latency regression and opens an issue with
  evidence.
- `AS-AGENT-03` A person creates a swarm from their agent run and steers the lead. The swarm
  finishes with its pull request linked, and the repository work list shows it alongside a
  finished independent run with its own pull request.

### 4.20 Organizations, enterprise, apps and billing

**Includes.**
- **Organizations:** orgs, teams (nested, with maintainers and review assignment), org and repo
  roles, custom roles, invitations, outside collaborators, member privileges.
- **Enterprise:** SAML SSO and SCIM provisioning; enterprise accounts with managed users; IP allow
  lists; verified domains; required 2FA.
- **Apps and marketplace:** the apps platform (manifests, installs, installation tokens); a
  marketplace for apps, actions and agents.
- **Billing:** usage metering, plans, budgets and spending limits; optional for self-hosters.
- **Model gateway:** prompt files and evals as repo content. Never on the critical path of source
  control.

**Done when.** SCIM deprovisioning removes access everywhere within the bound; an app installed on
two repos cannot touch a third.

**Agent scenarios.**
- `AS-ORG-01` Provision a team through SCIM, grant it a custom role on three repos, verify access.

### 4.21 Moderation and community

**Includes.**
- Blocking users; reports and an admin queue; interaction limits.
- Hiding and minimizing comments, with edit history; locking conversations.
- Moderator role; first-time contributor approval; code of conduct and community profile.
- Defenses against mass agent PR floods (rate limits, reputation, caps on open CRs).

**Done when.** Each moderation action is enforced at every write path and is audited; a simulated
agent flood against a public repo is contained without affecting other repos.

**Agent scenarios.**
- `AS-MOD-01` As a moderator agent, hide spam, block its author and set interaction limits.

### 4.22 Operations and reliability

**Includes.**
- **Configuration:** TOML config with strict validation and documented defaults.
- **Observability:** health endpoints, metrics with fixed names, structured logs without raw paths
  or secrets, tracing.
- **Admission control:** rate limiting with a published algorithm and cost model, priority tiers,
  quotas per delegation root.
- **Isolation:** cells with static stability; failure isolation between the git path and the
  forge.
- **Change safety:** rollouts by cell with automatic rollback; automatic certificates; an
  independent public status page.

**Done when.** Each isolation property has a fault test that proves it:
- forge database stopped: clone, checkout and land still work;
- search down: issue and CR lists still work;
- a cell lost: other cells unaffected;
- metadata primary down: reads served stale and marked stale.

**Agent scenarios.**
- `AS-OPS-01` As an operator agent, diagnose an injected fault from metrics, logs and traces and
  name the failing component.

### 4.23 Performance at agent scale

Targets (per cell, reference hardware; measured by the load harness every release):

- **Changesets:** one-file CreateChangeset p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size.
- **Landing:** uncontended land p50 ≤ 30 ms, p99 ≤ 150 ms. On one hot ref: ≥ 100 lands/s with
  1,000 concurrent landers, p99 time-to-land ≤ 10 s.
- **Checkout:** sparse 1,000 files ≤ 1 s warm, ≤ 3 s cold.
- **History:** log, blame and diff p99 ≤ 200 ms.
- **Events:** delivery p99 ≤ 1 s.
- **Concurrency:** ≥ 10,000 agent sessions with streams open.
- **Availability:** reads 99.99%, writes 99.95%.

**Done when.** The harness report for the release shows every target met, or lists each miss
with its measured value and an explicit waiver.

### 4.24 Web, mobile and desktop

**Includes.**
- A web app covering every area above, with rich rendering (Markdown, diagrams, math,
  notebooks, CSV, images, 3D, PDF), a web editor, conflict editor, accessibility and keyboard
  shortcuts.
- Change requests shown as briefs for people supervising agents: a Brief tab first (outcome,
  what needs you, evidence, risk, who did it), code on demand, and a review queue.
- A responsive mobile experience (installable PWA) for inbox, review and merge.
- Desktop: git compatibility makes existing git desktop clients work.

**Done when.** No operation is UI-only: everything the web app does is an API call that agents
can make (checked by the three-form conformance test). The UI passes accessibility checks.

### 4.25 Testing, conformance and auditability

**Includes.**
- **Correctness tooling:** reference model, golden vectors, fuzz targets for every decoder and
  parser (encoding, merge, pack parsing, policy, workflow YAML), fault points, fsck, complexity
  gates, linearizability.
- **Differential tests:** against git, `gh` and real workflows.
- **Conformance and agents:** the tier and API conformance suites; the agent scenario harness
  (§3.1).
- **Scale and resilience runs:** multi-day landing throughput, a two-week soak with faults, zone
  loss, restore drills, rolling upgrades, the largest-import test.
- **Traceability:** the coverage matrix and its checker.

**Done when.** CI runs everything except the multi-day runs on every change; the multi-day runs
gate each release; and the published release report lists every test, every agent scenario
result per runtime, and every waiver.

---

## 5. How we will know the whole product is done

1. `scripts/check-coverage.py` passes: every row in `docs/coverage/features.csv` maps to an area
   and a test, and every test it names exists.
2. The release report shows every agent scenario in §4 passing 3/3 on at least two agent
   runtimes.
3. The API, CLI, MCP, tier, git and GitHub-compatibility conformance suites pass in full.
4. The performance report meets §4.23 or lists each waiver.
5. The reliability fault tests in §4.22 pass.
6. A large public repository has been migrated in, worked on by agents through every surface,
   and exported back out, with a clean fidelity report both ways.

## 6. Build order

Everything ships; this is the order, so that each step is usable and tested before the next one
depends on it.

| Step | Areas | Usable outcome |
|---|---|---|
| 1. Core and contracts | 4.1–4.3, 4.7 (core commands), 4.8 (native API, MCP, events), 4.9 (tokens, policy), 4.25 (harness) | Agents commit, land and query history through API, CLI and MCP on `--dev` |
| 2. Git and migration | 4.11, 4.12 (GitHub mirror) | Any GitHub repo imports with identical commit ids; plain git works |
| 3. Review and CI | 4.13, 4.14, 4.4 (search), 4.19 (leases, agent runner) | A team of agents runs a real project: CRs, merge queue, workflows |
| 4. Scale | 4.5, 4.6 (virtual checkout, watcher), 4.22, 4.23 | Agent-scale load meets targets across cells |
| 5. Full forge | 4.10, 4.15–4.18, 4.20, 4.21, 4.24, GitHub API compatibility | Feature parity with GitHub, verified by the coverage matrix |
| 6. Done | §5 | The release report is green |

### 6.1 Releases

The open-source 0.1 release ships build steps 1 through 3. Section 5 remains the definition of
1.0; releases are milestones, not scope cuts. Each release publishes its agent-scenario results
and benchmark report. If a scenario or target is missed, it is fixed or listed as a known gap in
the release notes.

## 7. Decisions

Settled by the owner on 2026-10-17.

1. **Hosting: open source, self-hostable, and a cloud version.** Every capability in §4 ships
   in the Apache-2.0 release. The cloud runs the same binary and adds only what operating a
   service requires:
   - multi-tenant signup;
   - regions and data residency;
   - managed upgrades;
   - a hosted runner and workspace fleet;
   - billing collection.

   Self-hosters get the same runner and workspace software to run on their own machines. A
   feature that only works in the cloud is a bug. Both deployments are tested: the agent
   scenarios and conformance suites run against a self-hosted `bee-server` and against a
   cloud staging cell.
2. **Workflow compatibility: as seamless as possible.** The target is that existing
   `.github/workflows` run unmodified, with the same outcomes:
   - We match documented GitHub Actions behaviour, plus any undocumented behaviour that real
     workflows rely on.
   - Done means 100% of a published corpus of real workflows match their GitHub outcomes. The
     corpus is the workflows of the 500 most-starred repositories that use Actions, plus
     `kubernetes/kubernetes`.
   - Each divergence is either fixed or listed as tied to a GitHub-only service, with its
     Beehive equivalent named.
   - The same rule applies to the GitHub REST/GraphQL compatibility layer used by `gh`,
     Octokit and the stock runner.
3. **Reference repository for migration: `kubernetes/kubernetes`.** As of 2026-10-17 it has
   92,251 PRs, 49,850 issues, a 1.5 GB repository and 128k stars, so it exercises every
   migration path at scale. GitLab published a 76-hour import time for it, a public number to
   beat. `cli/cli` is the fast reference used in CI on every change; Kubernetes gates each
   release.
4. **Agent runtimes for the pass bar: Claude Code and Codex**, 3/3 runs each (§3.1). Other
   runtimes may be added; none may be removed without a recorded decision.
