# Gap audit: our spec v0.2 vs full GitHub (2026)

Generated 2026-10-17. Input for spec v0.3. Every item here must end up fully specified (no deferral).

# Beehive vs GitHub.com gap audit (2026-10-17)

This is a read-only audit; no files were edited. Sources read: `SPEC.md` (v0.2) and `proto/bee/v1/{vcs,forge,auth,common,import}.proto`. The inventory follows the docs.github.com table of contents (https://docs.github.com/en), plus the 2026 changelog.

**Key:** OK = specified. PARTIAL = some of it is specified, or the spec has it but the proto lacks the API. MISSING = not mentioned. EXCLUDED = a stated non-goal or deferral (§1.2, §11.1, §11.8, App. A).

## The six points that matter most

1. **The non-goals clash with the full-GitHub mandate.** §1.2 rules out built-in CI runners, package registries, static hosting, wikis and discussions. §11.1 defers forks to phase 3. §11.8 sends planning (boards, sprints, roadmaps) to outside trackers. Each of these needs a spec amendment.
2. **Whole areas are absent from the spec:** security (Dependabot, CodeQL-style scanning, secret scanning), Copilot-style agents, Codespaces, Gists, notifications, search, social features, Sponsors, Marketplace, GitHub Apps, a GitHub-compatible REST/GraphQL API, teams, SSO/SCIM, enterprise accounts, billing, moderation, Models and Spark.
3. **The spec and the proto disagree.** §11.8 defines issues, but there is no `IssueService`. §17.2 maps Releases, but there is no `ReleaseService`. There are no `DeleteRepo`, `DeleteOrg`, `ListWebhooks`, reopen-CR or plain CR comment RPCs.
4. **The design blocks repo transfer and tag protection.** §6.1 says "objects never move between orgs", which conflicts with repo transfer. The ref namespaces in §5 have no `tags/*`.
5. **Git over SSH is missing.** §17.4 covers smart HTTP only.
6. **The biggest migration risk is the API surface.** There is no GitHub REST/GraphQL compatibility, so `gh`, Octokit, Actions and the app ecosystem won't work against Beehive without a compat layer.

---

## 0. Spec-internal gaps (fix first)

- [PARTIAL] Issues: in §11.8 but no `IssueService` in the proto. Add `IssueService` (CRUD, comments, labels, milestones, assignees).
- [PARTIAL] Releases: mapped in §17.2, no section, no proto. Add `ReleaseService` plus asset storage in the blob store.
- [PARTIAL] Webhooks: Create/Delete only. Add List, Update, Ping, delivery log and redeliver.
- [MISSING] Delete repo, delete org, rename org. Add RPCs with soft-delete and a restore window.
- [MISSING] CR reopen, CR edit (title/body/target), and comments outside a review. Extend `ChangeService`.
- [MISSING] Policy has no read API (file only). Add `GetEffectivePolicy` and `ValidatePolicy` RPCs.
- [PARTIAL] Repo settings: `settings` is in §11.1 but not in the `Repo` message. Add a typed settings message.

---

## 1. Repositories (https://docs.github.com/en/repositories)

- [OK] Create repo: §11.1, `CreateRepo`.
- [OK] Visibility (private, internal, public): §11.1.
- [OK] Rename with redirects (90 days): §11.1.
- [OK] Description: proto `Repo.description`.
- [PARTIAL] Repo delete and restore: not in the proto. Soft-delete with a 90-day restore.
- [MISSING] Archive (read-only): add an `archived` flag. Policy then denies all writes, Land and leases.
- [MISSING] Transfer between owners or orgs: conflicts with §6.1. Copy objects into the target org (dedup), keep the repo ULID and redirect.
- [MISSING] Template repos and "use this template": mark a repo as a template, then create a new repo from its tip with a fresh root changeset.
- [MISSING] Topics: a repo tags table plus topic pages.
- [MISSING] Homepage URL and social preview image: repo metadata fields plus a stored blob.
- [MISSING] License detection and picker: a derived type that runs licensee-style matching on LICENSE files.
- [MISSING] README, CONTRIBUTING, SECURITY.md, CODE_OF_CONDUCT, FUNDING.yml, SUPPORT discovery: derived "community files" lookup (repo root, then `.github`, then `docs`).
- [MISSING] Citation files (CITATION.cff, "Cite this repository"): parse CITATION.cff and emit APA and BibTeX.
- [MISSING] Language stats (linguist, `.gitattributes` overrides): a lazy derived type over the tree.
- [MISSING] Insights (pulse, contributors, commits, code frequency, traffic, clones, referrers, popular content, dependency graph, network, forks): derived aggregates from the ref-log, `path_history` and access logs, exposed as APIs.
- [PARTIAL] Contributors list: computable from Log and provenance, but not specified. Add a derived contributor index that maps placeholder principals.
- [MISSING] Network graph: a graph API over changesets across forks and refs.
- [MISSING] Rulesets as a distinct object (repo and org level, bypass lists, evaluate mode, rule insights): §9.4 policy covers the semantics. Add bypass actors, a dry-run/evaluate mode and a decision log.
- [OK] Branch protection: land rules, required checks, required approvals (§9.4, §5).
- [OK] Require linear history: inherent, since Land rebases (§7.4).
- [OK] Lock branch and restrict who can push: policy land rules.
- [MISSING] Require signed commits: verify gpgsig/SSH signatures and add a `require_signed` land rule.
- [MISSING] Tag protection and tag rulesets: add `tags/*` refs governed by policy.
- [PARTIAL] Push rules (path, extension, file size, path length restrictions): blob limits (§4.6) and write rules by path exist. Add deny-by-glob, extension and size rules.
- [MISSING] Required deployments before merge: needs deployments (§3).
- [MISSING] Required workflows (rulesets): needs the workflow engine. Policy names workflows that must pass.
- [MISSING] Required code scanning results (ruleset): needs code scanning.
- [MISSING] Copilot automatic review in rulesets: a policy hook that requests an agent review on CR open.
- [MISSING] Autolinks (e.g. `JIRA-123` to a URL): a per-repo regex-to-URL table applied when rendering.
- [MISSING] Custom properties (org-defined repo metadata): typed key/value pairs on repos that policy can match on.
- [PARTIAL] Git LFS: imported (§17.1) but not served. Add an LFS batch API endpoint backed by the blob store, or map LFS pointers to native large blobs.
- [PARTIAL] Releases (notes, assets, drafts, prereleases, latest, generated notes, immutable releases, discussions link): imported only. Add a native object that points at a tag, with asset blobs and auto-generated notes from the CRs in range.
- [PARTIAL] Tags (lightweight and annotated): imported through git (§17.4), but §5 has no tag namespace. Add `tags/*` refs and an annotated-tag object.
- [EXCLUDED] Wikis: §1.2. Back each wiki with a sibling repo `<repo>.wiki`, rendered as markdown and cloneable.
- [EXCLUDED] Forks (phase 3, §11.1, App. A.5): per-repo `object_paths` plus a fork-point check. Fork-sync, cross-fork CRs.
- [MISSING] Fork sync ("Sync fork") and contribute: the Rebase/Merge RPC against the upstream ref.
- [OK] Branches (list, create, delete, rename): `RefService`. Rename is not explicit; add it.
- [OK] Default branch: `default_ref`.
- [MISSING] Collaborators and outside collaborators, invitations: repo-level grants through policy groups, plus an invitation flow.
- [MISSING] Repo roles (read, triage, write, maintain, admin): define them as built-in policy groups.
- [MISSING] Deploy keys: replaced by service principals and scoped Biscuit tokens. Provide SSH deploy keys for compatibility.
- [MISSING] Source archive downloads (zip and tar.gz per ref): a streaming archive endpoint from trees, cacheable by tree id.
- [OK] Submodules: gitlink mode (§4.2).
- [OK] Symlinks: §4.2, §10.3.
- [OK] Blame, history, raw, permalinks: `QueryService`. Content ids give stable permalinks.
- [MISSING] `.git-blame-ignore-revs`: honor it in the blame derived type.
- [MISSING] Rich rendering (Markdown/GFM, Mermaid, math, GeoJSON/TopoJSON, STL, Jupyter, CSV, PDF, image diff): a web rendering pipeline with sanitized output.
- [MISSING] Code navigation (symbols, jump to definition, references): a derived symbol index with tree-sitter or stack-graphs.
- [MISSING] Web file editor, upload and delete through the UI: the API exists (CreateChangeset). Needs a web UI spec.
- [PARTIAL] Repo mirroring (push mirror): only during GitHub cutover (§17.5 C). Generalize it to any git remote, both directions, as a setting.
- [MISSING] Repository size and health (counts, large-file warnings): admin APIs.
- [MISSING] Merge button settings (allowed methods, default commit message templates, delete branch on merge, suggest updating branches): repo settings fields.

## 2. Pull requests / change requests (https://docs.github.com/en/pull-requests)

- [OK] Open, close, list: §11.2, `ChangeService`.
- [PARTIAL] Reopen and edit title, body or target: not in the proto. Add RPCs.
- [OK] Revisions ("changes since last review"): §11.2. Better than GitHub.
- [OK] Stacked PRs (GA 2026-10-06): `depends_on` (§11.2). Add stack rebase, auto-retarget and stack-into-queue parity.
- [MISSING] Draft PRs and "ready for review": add a `draft` state that policy cannot land.
- [MISSING] Requested reviewers (users and teams), auto-assignment (round robin, load balance): a `review_requests` table plus team auto-assignment.
- [OK] CODEOWNERS: owners in policy, file kept (§9.4, §17.2). Also parse `CODEOWNERS` natively so the file stays authoritative if users choose.
- [OK] Line and multi-line comments: anchored to a line range (§11.4).
- [MISSING] File-level comments: anchor with no line range.
- [PARTIAL] Review threads (replies, resolve/unresolve, outdated marking): carry-forward exists (§11.4). Add thread replies and `resolved` state. Policy can require all threads resolved.
- [PARTIAL] Suggested changes (apply suggestion, batch apply): imported only. Add a structured `suggestion` payload and "apply" that creates a changeset on the CR.
- [OK] Review verdicts (approve, request changes, comment): §11.4.
- [MISSING] Dismiss review and stale-approval dismissal on push: a policy flag, since reviews attach to revisions.
- [MISSING] Require approval of the most recent push by someone other than the pusher: a policy rule.
- [PARTIAL] Conversation comments (top-level timeline): only review bodies exist. Add CR comments.
- [PARTIAL] Reactions: archived only. Native reactions on comments, CRs and issues.
- [PARTIAL] Merge methods (merge commit, squash, rebase): only rebase-land. Merges in a stack are rejected (§7.4). Add squash, plus merge-commit land as a policy option.
- [MISSING] Auto-merge (land when requirements are met): a queued land intent that fires on `check.updated` or `review.submitted`.
- [PARTIAL] Merge queue (grouping, required checks on merge groups, jump the queue, size limits): landing trains (§7.4, phase 2). Add a pre-land check run on the train result, queue UI and priority.
- [OK] Required status checks: §9.4, §11.6.
- [PARTIAL] Status checks (check suites, annotations, re-run, actions buttons, output markdown, external IDs): CheckRun is minimal. Extend it with suites, annotations, rerequest and output.
- [MISSING] Commit statuses API (legacy `state` + `context`): a compat shim mapped onto CheckRun.
- [MISSING] Deployments and environments on PRs: a deployments API (§5).
- [OK] Update branch: the Rebase RPC (§7.3).
- [OK] Conflict resolution: structured conflicts plus resolver hook (§7.3.2, §11.6). Add a web conflict editor.
- [PARTIAL] Revert button: automatic revert CR after post-land failure (§11.6). Add a manual "revert this CR" action.
- [MISSING] Linked issues and closing keywords (`Fixes #12`): parse the CR body and changeset messages, link them, close on land.
- [MISSING] Labels, assignees and milestones on CRs: share the issue metadata model.
- [MISSING] PR templates (`.github/pull_request_template.md`, multiple templates): read them from the target ref.
- [MISSING] "Viewed" file state per reviewer: per (reviewer, revision, path) state.
- [MISSING] Compare view across arbitrary refs and forks: Diff exists. Add compare-and-open-CR.
- [PARTIAL] Diff options (split/unified, whitespace ignore, rename detection, large-diff handling): rename detection is open (App. A.3). Add the options as Diff request flags.
- [MISSING] Commit comments: comments anchored to a changeset outside a CR.
- [MISSING] Locking conversations on a CR: needs moderation (§19).
- [MISSING] Copilot PR summaries and agent reviews: agents can review (§11.4). Add a summary field authored by an agent principal.
- [OK] Pull request refs (`refs/pull/N/head`): imported. Serve them over git compatibility.
- [OK] Push-to-open-CR (`push.autoChange`): §17.4.

## 3. Deployments and environments (https://docs.github.com/en/actions/deployment)

- [MISSING] Deployments API (statuses, environments, transient and production, deployment history): native objects keyed to a changeset, with events.
- [MISSING] Environments (protection rules, required reviewers, wait timers, branch restrictions, custom protection rules via apps): policy-governed environments, where approval is a review on the deployment.
- [MISSING] Environment secrets and variables: scoped secrets in the secret store (§5).

## 4. Issues (https://docs.github.com/en/issues)

- [PARTIAL] Issues CRUD, comments, number preservation: §11.8. No proto.
- [PARTIAL] Labels (create, color, description, default set): in the model, but no management API.
- [PARTIAL] Milestones (due date, progress): field only. Add a milestone object.
- [OK] Assignees: §11.8.
- [MISSING] Issue types (org-defined): an org-level type table.
- [MISSING] Sub-issues (hierarchy, progress): a parent/child relation.
- [MISSING] Issue dependencies (blocked by / blocking): a relation table.
- [MISSING] Issue fields (GA 2026-07: Priority, Effort, Start/Target date, custom): typed org-defined fields on issues.
- [MISSING] Issue templates and issue forms (YAML), config.yml, contact links: parsed from the default ref.
- [PARTIAL] Linked PRs and development panel: "CR can point at the task" only. Add an explicit link table plus branch/CR creation from an issue.
- [MISSING] Closing keywords: see §2.
- [MISSING] Pinned issues: a per-repo pin list.
- [MISSING] Locked issues (with reason): needs moderation.
- [MISSING] Transfer an issue between repos: move the issue and keep a redirect.
- [MISSING] Mark as duplicate, close reasons (completed, not planned, duplicate): `state_reason`.
- [MISSING] Task lists and checkboxes progress: markdown-derived.
- [MISSING] Mentions and cross-references timeline events: a timeline event table plus notifications.
- [PARTIAL] Reactions: archived only.
- [MISSING] Saved replies: per-user snippets.
- [MISSING] Assign an issue to an agent (Copilot coding agent pattern): assignee is an agent principal, which triggers an agent session (§10).
- [MISSING] Issue search syntax: see §14.

## 5. Projects v2 (https://docs.github.com/en/issues/planning-and-tracking-with-projects)

- [EXCLUDED] Projects (table, board and roadmap views, custom fields, iterations, grouping, slicing, filters, workflows/automations, insights charts, templates, status updates, archive, public projects with MCP access): §11.8 sends planning to external trackers, and §17.2 archives Projects read-only. Build native Projects: items are issues, CRs or drafts with typed fields, saved views and event-driven automations, exposed over API and MCP.

## 6. Discussions (https://docs.github.com/en/discussions)

- [EXCLUDED] Discussions (categories, Q&A answers, polls, announcements, discussion forms, convert issue to discussion, org discussions, spotlight, labels): §1.2, and §17.2 archives them. Build a native threaded object that reuses the comment and reaction models.

## 7. Actions (https://docs.github.com/en/actions)

The whole area is EXCLUDED by §1.2 ("built-in CI runners"). App. A.7 leaves open whether to run workflows compatibly or bridge to GitHub-hosted runners. Recommendation: a native workflow engine that runs `.github/workflows` YAML compatibly, triggered by the event log (§11.7). Its results become CheckRuns.

- [EXCLUDED] Workflow syntax (on, jobs, steps, needs, if, expressions, contexts, defaults, permissions): an act-compatible YAML engine with a conformance test suite.
- [EXCLUDED] Triggers (push, pull_request, pull_request_target, schedule, workflow_dispatch, repository_dispatch, workflow_run, merge_group, release, issue events, and more): map them from Beehive events.
- [EXCLUDED] GitHub-hosted runners (Linux, Windows, macOS, ARM, GPU): runner pools in cells; Firecracker/VM sandboxes.
- [EXCLUDED] Larger runners and static IPs: sized runner classes.
- [EXCLUDED] Self-hosted runners, runner groups, Actions Runner Controller: speak the actions/runner protocol so stock runners register.
- [EXCLUDED] Custom runner images: an OCI image per runner class.
- [EXCLUDED] Reusable workflows (`workflow_call`): resolved by repo@ref.
- [EXCLUDED] Composite, JavaScript and Docker actions: run `uses:` actions from a mirrored registry.
- [EXCLUDED] Actions Marketplace: an action index (§17).
- [EXCLUDED] Caches (`actions/cache` API): a cache service on the blob store, scoped by ref.
- [EXCLUDED] Artifacts (upload-artifact v4 API, retention): an artifact service on the blob store.
- [EXCLUDED] Secrets (repo, env, org), variables, GITHUB_TOKEN with permissions: a secret store, plus GITHUB_TOKEN minted as a scoped Biscuit token.
- [EXCLUDED] OIDC tokens to clouds: an OIDC issuer with GitHub-compatible claims.
- [EXCLUDED] Concurrency groups, cancel-in-progress: engine scheduler.
- [EXCLUDED] Matrix (include, exclude, fail-fast, max-parallel): engine.
- [EXCLUDED] Services containers, container jobs: engine.
- [EXCLUDED] Job summaries, logs (live, download), annotations: CheckRun output plus a log store.
- [EXCLUDED] Re-run (failed jobs, with debug logging): rerequest on CheckRun.
- [EXCLUDED] Environments and deployment protection: see §3.
- [EXCLUDED] Artifact attestations and build provenance (Sigstore): a native attestation service. This fits the provenance theme.
- [EXCLUDED] Actions policies (allowed actions list, fork PR approval, workflow permissions defaults): org and repo policy.
- [EXCLUDED] Required workflows and org rulesets: policy.
- [EXCLUDED] Starter workflows, usage metrics, billing minutes: needs billing (§21).
- [PARTIAL] External CI reporting results: CheckService (§11.6), plus `green/*` refs (§5).

## 8. Packages and container registry (https://docs.github.com/en/packages)

- [EXCLUDED] Container registry (OCI, ghcr.io equivalent): §1.2. An OCI distribution-spec registry on the blob store.
- [EXCLUDED] npm registry: an npm registry API.
- [EXCLUDED] Maven and Gradle: a Maven repository layout.
- [EXCLUDED] NuGet: NuGet v3 API.
- [EXCLUDED] RubyGems: gem server API.
- [EXCLUDED] Package visibility, permissions, linking to a repo, versions, deletion, restore: one package model across registries, authorized through policy and tokens.
- [EXCLUDED] Publishing from workflows with GITHUB_TOKEN: Biscuit-scoped package write.
- [MISSING] Package provenance and attestations: tie them to changeset provenance.

## 9. Pages (https://docs.github.com/en/pages)

- [EXCLUDED] Static hosting from a ref or folder, or from a workflow artifact: §1.2. Serve a tree id from a CDN, keyed by ref tip.
- [EXCLUDED] Custom domains, HTTPS certificates, domain verification: ACME automation (§15.6 already covers certificates).
- [EXCLUDED] Jekyll builds: run as a workflow.
- [EXCLUDED] Private (access-controlled) Pages: auth through repo read policy.

## 10. Codespaces (https://docs.github.com/en/codespaces)

- [MISSING] Cloud dev environments (devcontainer.json, machine types, prebuilds, secrets, port forwarding, dotfiles, VS Code/JetBrains/browser, idle timeouts, org policies, billing): agent sandboxes. Run devcontainers against a sparse working copy plus a node cache (§10). The same infrastructure serves human Codespaces and the coding agent.

## 11. Copilot and agents (https://docs.github.com/en/copilot)

- [MISSING] Copilot coding agent (assign an issue, it works in a sandbox, opens a PR, iterates on review comments, session logs): a native agent runner. Agent principal, Biscuit token, provenance session (§9, §10.4), leases.
- [MISSING] Agent HQ / Mission Control (third-party agents such as Claude and Codex, agent identity, metrics dashboard, Slack and Linear integrations): an agent registry. Any runtime can register; per-agent dashboards come from provenance cost and outcomes.
- [PARTIAL] Agent identity and permissions: agent principals with owners, attenuated budgeted tokens (§9.1, §9.2). Better than GitHub.
- [OK] Agent provenance on commits: §10.4.
- [OK] MCP server: §12.4. GitHub's MCP server equivalent.
- [MISSING] Custom agents and instructions (`.github/copilot-instructions.md`, AGENTS.md, `.github/agents/*.md`, prompt files): read from the repo, expose through `GetContext`.
- [MISSING] Copilot code review (automatic PR review): an agent reviewer triggered by policy (§11.4).
- [MISSING] Copilot Chat on the web, PR summaries, commit message generation: optional model-backed features. They must stay off the critical path (§15.6).
- [MISSING] Copilot Spaces (curated context): saved context bundles (paths, issues, docs) over the API.
- [MISSING] Copilot Memory: a per-repo agent memory store.
- [MISSING] Copilot Autofix for code scanning: an agent opens a fix CR from an alert.
- [MISSING] Agent firewall and network allow-list for agent sandboxes: sandbox egress policy.
- [MISSING] Copilot policies, seats, usage metrics, premium requests and BYOK: org settings plus billing. Provider-agnostic.
- [PARTIAL] One-click conflict resolution by agent: resolver hook (§11.6).
- [MISSING] Copilot code completions and IDE extensions: out of scope for a forge. Expose a model gateway or leave it to IDE vendors.

## 12. Security and code quality (https://docs.github.com/en/code-security)

- [MISSING] Dependency graph (manifests, lockfiles, submission API): a derived type parsing manifests. `diff_class=dependency` already detects them.
- [MISSING] SBOM export (SPDX, CycloneDX): generated from the dependency graph.
- [MISSING] Dependabot alerts: match the dependency graph against an advisory DB (OSV, GHSA mirror).
- [MISSING] Dependabot security and version updates (`dependabot.yml`, grouped updates): a bot agent principal opens CRs. It can run Renovate or Dependabot-core.
- [MISSING] Dependency review (on CRs, plus the action): a diff of dependency graphs between revisions, as a required check.
- [MISSING] Code scanning (CodeQL, SARIF upload, alerts, PR annotations, default and advanced setup): SARIF ingestion API, alerts table, check annotations. Run CodeQL or Semgrep through workflows.
- [MISSING] Secret scanning (partner patterns, custom patterns, validity checks, AI generic detection): a scanner on ingest (blob-level, done once per org thanks to dedup).
- [MISSING] Push protection (blocks pushes, delegated bypass): scan at CreateChangeset and Land, reject with `SECRET_DETECTED`, bypass through policy.
- [PARTIAL] Leaked-secret remediation (purge history): redaction (§13). Better than GitHub's history rewrite.
- [MISSING] Repository security advisories (GHSA drafts, CVE requests, temporary private forks): an advisory object plus private-fork mechanics (needs forks).
- [MISSING] Global advisory database: mirror OSV or GHSA.
- [MISSING] Private vulnerability reporting: a private report intake that becomes an advisory draft.
- [MISSING] Security overview (org dashboards, risk and coverage): aggregation APIs.
- [MISSING] Security campaigns: bulk alert remediation assigned to agents.
- [MISSING] GitHub Code Quality (GA 2026): quality findings as alerts and checks.
- [MISSING] Artifact attestations (Sigstore): see §7.
- [MISSING] Vigilant mode and signature verification badges: verify gpgsig, SSH and S/MIME signatures. Show provenance alongside.
- [MISSING] SECURITY.md and security policy surfacing: community files.

## 13. Gists (https://docs.github.com/en/get-started/writing-on-github/editing-and-sharing-content-with-gists)

- [MISSING] Gists (public and secret, multi-file, revisions, forks, stars, embed, clone): a lightweight user-owned repo kind, cloneable over git compatibility.

## 14. Search (https://docs.github.com/en/search-github)

- [PARTIAL] Full-text search: §15.1 mentions it as optional enrichment only.
- [MISSING] Code search (regex, symbols, path/language qualifiers, cross-repo): a trigram or zoekt index built from the content-addressed blob store, with path ACLs.
- [MISSING] Issue and PR search syntax (`is:`, `author:`, `label:`, `review:`, `status:`, ...): a query language over forge tables, with GitHub-compatible qualifiers.
- [MISSING] Repo, commit, user, topic, discussion and package search: per-entity indexes.
- [MISSING] Saved searches and filters: per-user.

## 15. Notifications (https://docs.github.com/en/subscriptions-and-notifications)

- [PARTIAL] Event stream and webhooks: §11.7. A machine feed, not per-user notifications.
- [MISSING] Inbox (participating, mentions, review requested, done, saved, custom filters): a per-principal notification table fed from events.
- [MISSING] Watching (all activity, custom, ignore), subscribe and unsubscribe to a thread: subscription model.
- [MISSING] Email notifications, reply-by-email, routing per org: email delivery worker.
- [MISSING] Scheduled reminders (Slack and Teams): integration worker.

## 16. Profiles and accounts (https://docs.github.com/en/account-and-profile)

- [PARTIAL] User principal (display name, external ids): §9.1.
- [MISSING] Profile page, profile README, avatar, bio, status, pronouns, pinned repos, followers and following, org badges: profile model plus UI.
- [MISSING] Contributions graph and activity overview: aggregated from the ref-log and events.
- [MISSING] Achievements: optional. Derive from events.
- [MISSING] Emails, noreply address, commit email mapping, email privacy: needed for git commit attribution.
- [MISSING] Account deletion, data export (user migration archive), username change: account lifecycle APIs.
- [MISSING] Settings (themes, accessibility, keyboard shortcuts, sessions, security log): per-user settings plus a user audit log.

## 17. Social and discovery (https://docs.github.com/en/get-started/exploring-projects-on-github)

- [MISSING] Stars and star lists: per-user tables.
- [MISSING] Watching: see §15.
- [MISSING] Explore, Trending, Topics pages, Collections: ranking jobs over stars and events.
- [MISSING] Following and the home feed: feed derived from events.

## 18. Sponsors and Marketplace (https://docs.github.com/en/sponsors, https://docs.github.com/en/apps/github-marketplace)

- [MISSING] Sponsors (tiers, one-time and monthly, fiscal hosts, sponsor-only content, FUNDING.yml button): build on Stripe Connect or integrate Open Collective. At minimum, render FUNDING.yml.
- [MISSING] Marketplace (apps and actions listing, paid plans, verified publishers): a registry of apps, actions and agents. Agents are listable too.

## 19. Apps, auth and APIs (https://docs.github.com/en/apps, https://docs.github.com/en/rest, https://docs.github.com/en/graphql, https://docs.github.com/en/webhooks, https://docs.github.com/en/authentication)

- [MISSING] GitHub Apps (installations, per-installation permissions, installation tokens, app manifests, user-to-server tokens): an app is a service principal plus an installation-scoped Biscuit. Add a manifest and an install flow.
- [MISSING] OAuth apps (authorize flow, scopes, device flow for third parties): an OAuth 2 / OIDC provider issuing Biscuit tokens.
- [OK] Fine-grained PATs: Biscuit delegated tokens (§9.2). Repo, path and action scoped with budgets, which goes beyond fine-grained PATs.
- [MISSING] Classic PATs and git credential compatibility: accept a Biscuit as the HTTP basic-auth password. Translate classic scopes.
- [MISSING] SSH keys (auth and git over SSH): an SSH git transport mapped to principals.
- [MISSING] GPG and SSH signing keys: needed for signature verification.
- [MISSING] Passwords, 2FA (TOTP, passkeys, security keys), sudo mode: optional local auth. GitHub/OIDC login is specified (§9.1).
- [PARTIAL] Webhooks (repo level, HMAC, retries): §11.7. Add org-level and app webhooks, the GitHub-compatible payload format (`X-GitHub-Event`), delivery logs and redelivery.
- [MISSING] GitHub REST API compatibility (`/repos/{o}/{r}/pulls`, ...): a compat gateway that translates to Connect services. Critical for migration, Octokit, gh and Actions.
- [MISSING] GraphQL API compatibility: a GraphQL facade over the same services. At least cover the subset gh and Projects use.
- [OK] Native typed API (Connect, gRPC, JSON): §12.
- [OK] Rate limits, retry-after, per-delegation-root quotas: §15.4.
- [MISSING] API versioning headers and REST deprecation policy: define them for the compat layer.
- [PARTIAL] Login with GitHub device flow and OIDC: §9.1.

## 20. Organizations, teams and enterprise (https://docs.github.com/en/organizations, https://docs.github.com/en/admin)

- [OK] Orgs and members: §11.1, `OrgService`.
- [PARTIAL] Org roles: only owner, admin and member. Add billing manager, security manager, moderator, app manager and custom org roles.
- [MISSING] Teams, nested teams, team maintainers, team sync, team discussions, team review assignment: first-class teams mapped to policy groups (§9.4 has groups).
- [MISSING] Custom repository roles: named permission bundles in policy.
- [MISSING] Invitations (pending, expiry), outside collaborators, member privileges (base permissions, who can create repos, fork policy): org settings.
- [PARTIAL] SSO: OIDC login (§9.1). Add SAML SSO per org and enterprise, with session enforcement.
- [MISSING] SCIM provisioning: a SCIM 2.0 endpoint that creates and removes principals and teams.
- [MISSING] Enterprise accounts (multi-org, enterprise policies, EMU managed users, enterprise owners): an enterprise layer above orgs. Can be cell-aligned (§15.2).
- [PARTIAL] Audit log: the ref-log (§5) and event log (§11.7) are per repo. Add an org and enterprise audit log covering settings, auth and policy, with streaming to SIEM (S3, Splunk, Datadog).
- [MISSING] IP allow lists: an org setting enforced at admission (§15.4).
- [MISSING] Verified and approved domains: DNS TXT verification.
- [MISSING] Require 2FA for members: depends on the 2FA item in §19.
- [MISSING] Org profile, README and pinned repos: profile model.
- [MISSING] Org-level webhooks, secrets, variables, runners: see §7 and §19.
- [OK] Data residency and self-hosting (GHE.com, GHES equivalent): cells (§15.2) plus a single binary.
- [PARTIAL] Repo policies at org level: org default policy (§9.4).

## 21. Billing (https://docs.github.com/en/billing)

- [MISSING] Plans, seats, metered usage (Actions minutes, storage, Codespaces, Copilot, LFS), budgets and spending limits, invoices, cost centers: a usage metering pipeline from the event log. Quotas already exist (§15.4). Billing is optional for self-hosted installs.

## 22. Client tools (https://docs.github.com/en/github-cli, https://docs.github.com/en/desktop, https://docs.github.com/en/get-started/using-github/github-mobile)

- [OK] Native CLI with `--json` and schemas: §12.4.
- [MISSING] `gh` compatibility (pr, issue, run, release, repo, api, extensions such as gh-stack): the REST/GraphQL compat layer lets `gh` work with `GH_HOST=beehive`. Ship `gh bee` as well.
- [PARTIAL] git CLI compatibility: smart HTTP (§17.4). SSH is missing.
- [MISSING] Desktop app: git compatibility is enough for GitHub Desktop pointed at Beehive. A native app is optional.
- [MISSING] Mobile apps (inbox, review, merge): a responsive web/PWA first.
- [MISSING] Web UI specification: React is named (§2) but nothing more is specified. Needs a UI spec per area.
- [EXCLUDED] Windows clients: §1.2.

## 23. Moderation and community (https://docs.github.com/en/communities)

- [MISSING] Block users (personal and org): block table, enforced at write paths.
- [MISSING] Report content and abuse: report intake plus admin queue.
- [MISSING] Interaction limits (temporary, existing users only, prior contributors): repo and org setting enforced in comment and CR creation.
- [MISSING] Hide or minimize comments (spam, off-topic, outdated, resolved), edit history, delete: comment moderation fields plus edit history.
- [MISSING] Lock conversations: lock flag on issues and CRs.
- [MISSING] Code of conduct and community profile checklist: derived community files.
- [MISSING] Moderator role: org role.
- [MISSING] First-time contributor handling (approve workflow runs, labels): policy plus the workflow engine.
- [PARTIAL] Content removal (DMCA, legal): redaction (§13) for blobs. Add a takedown process for repos and comments.

## 24. Migrations (https://docs.github.com/en/migrations)

- [OK] GitHub importer (one command, dry run, live sync, dual run, fidelity report): §17.
- [OK] Identity claiming: §17.2.
- [MISSING] Importers from GitLab, Bitbucket, Azure DevOps and Gitea: reuse the §17 pipeline with per-source adapters.
- [MISSING] Export (migration archive out, push to GitHub at any time): an org and repo export in GitHub migration-archive format. This keeps migration easy in both directions.
- [PARTIAL] Secrets, deploy keys and webhooks re-entry checklist: §17.2.

## 25. Models, Spark and other 2025-26 products (https://docs.github.com/en/github-models, https://docs.github.com/en/copilot)

- [MISSING] GitHub Models (catalog, playground, prompt files `.prompt.yml`, evals, inference API with BYOK): a model gateway plus versioned prompt files as repo content, with evals as checks. Off the critical path (§15.6).
- [MISSING] GitHub Spark (natural-language app builder that hosts apps): agent-built apps deployed through Pages and hosting plus a managed runtime.
- [MISSING] Education and Nonprofit programs: business programs, not product features. Mark them N/A or handle as a billing discount.
- [OK] Status and availability commitments: SLOs (§16), cells (§15).
- [MISSING] Public status page: the operations item in §15.6 needs an independent status page.
- [MISSING] Org dependency insights: org-wide aggregation of the dependency graph.
- [MISSING] Slack, Teams, Jira and Linear integrations: event-subscriber apps on webhooks or events (§11.7).

---

**What the spec does better than GitHub:** server-side land without a global lock; landing trains; path leases; structured conflicts plus the resolver hook; semantic diff classes driving approvals; agent principals with attenuated, budgeted tokens; provenance on every changeset; redaction that changes no ids; history and blame APIs that need no clone; per-delegation-root quotas; cell isolation.

**Sources:**
- Docs backbone: https://docs.github.com/en (section URLs listed per area above)
- [Stacked PRs GA 2026-10-06 (gigazine)](https://gigazine.net/gsc_news/en/20260416-github-stacked-pr/)
- [Stacked PRs guide (betterstack)](https://betterstack.com/community/guides/linux/github-stacked/)
- [Stacked PRs in public preview (releases.sh)](https://releases.sh/release/rel__gf_lG_XlSLmWEsyYUEwN-stacked-pull-requests-in-public-preview)
- [Agent HQ announcement (SD Times)](https://sdtimes.com/ai/github-unveils-agent-hq-the-next-evolution-of-its-platform-that-focuses-on-agent-based-development/)
- [Copilot coding agent capabilities in 2026 (morphllm)](https://www.morphllm.com/copilot-coding-agent)
- [Issue fields GA 2026-07-02](https://github.blog/changelog/2026-07-02-issue-fields-are-now-generally-available)
- [Issue fields GA and Code Quality GA (releases.sh)](https://releases.sh/release/rel_M-Gtc1AAD2HnbwuYUoVRo-issue-fields-ga-structured-metadata-public-projects-mcp-access)

The Cloudflare MCP plugin needs authorization through `claude mcp` or `/mcp` in an interactive session before its tools can be used. This audit didn't need it.