# Plan

> The product build order is `docs/PRODUCT.md` §6, and everything in PRODUCT.md §4 ships.
> The phases below are the original detail for build steps 1 and 2; later steps get task
> tables before they start. Nothing below is a scope cut.

Four phases. Each one ends with something you can run.

| Phase | Ends when |
|---|---|
| 0. Contracts | SPEC.md, `proto/bee/v1`, schemas and this plan are reviewed; the workspace builds |
| 1. VCS core | bee hosts its own repository: commit, land, log, diff, blame, sparse checkout, agent tokens, MCP, `git clone`/`push` over smart HTTP, and `bee import github` in mirror mode; the load harness measures every SPEC §16 target |
| 2. Beehive | change requests, diff classes, reviews, checks, leases, events, landing trains, resolver hook, auto-revert, web UI, GitHub bridge and cutover, PR/issue link redirects |
| 3. Scale and reach | multiple cells with the cell directory, node daemon snapshots, regional cache, virtual checkout, code search, forks, Actions compatibility, public launch |

## Crate layers

A crate may depend only on crates in a lower layer (`scripts/layers.toml`, checked by
`scripts/check.sh`).

| Layer | Crates |
|---|---|
| 0 | `bee-core` |
| 1 | `bee-object`, `bee-merge`, `bee-policy`, `bee-classify`, `bee-secrets`, `bee-workflow`, `bee-deps` |
| 2 | `bee-store`, `bee-proto`, `bee-auth`, `bee-git` |
| 3 | `bee-store-sql`, `bee-store-blob`, `bee-history`, `bee-import`, `bee-testkit`, `bee-wc`, `bee-daemon`, `bee-mcp` |
| 4 | `bee-derived`, `bee-search` |
| 5 | `bee-forge` |
| 6 | `bee-conformance`, `bee-cli`, `bee-github`, `bee-git-http`, `bee-compat` |
| 7 | `bee-server` |

## Phase 1 tasks

Each task is sized for one worker on its own branch. **Owns** lists the only paths the task may
edit; anything else is a request to the lead. Every task ships tests that run under
`cargo test --workspace` with no external services.

### Wave 0: foundation (one worker, lands first)

| # | Task | Owns | Done when |
|---|---|---|---|
| F1 | Domain types and canonical encoding | `crates/bee-core`, `crates/bee-object`, `testdata/vectors` | Ids, kinds, text forms, path rules, limits, error reasons; sCBOR encode/decode for tree, meta, changeset with every rejection rule in SPEC §4; golden vectors frozen; invariant D checker; tree-from-delta builder |
| F2 | Protocol codegen | `crates/bee-proto`, `proto/` (additive only) | `bee-proto` generates Rust from `proto/bee/v1` in `build.rs`; conversions to and from `bee-object` types round-trip; transport choice from SPEC Appendix A.1 decided and recorded |

F1 and F2 can run in parallel; F2's conversions merge after F1.

### Wave 1: parallel (start once F1 lands)

| # | Task | Owns | Depends on |
|---|---|---|---|
| W1 | Store traits, in-memory store, fault points, fsck | `crates/bee-store`, `crates/bee-testkit` | F1 |
| W2 | diff3-v1 merge, conflict classification, `docs/contracts/diff3-v1.md` | `crates/bee-merge`, `docs/contracts/diff3-v1.md` | F1 |
| W3 | Policy parser and evaluator (access, owners, land rules) | `crates/bee-policy` | F1 |
| W4 | Principals and Biscuit tokens: mint, attenuate, verify, revoke, budgets | `crates/bee-auth` | F1, W3 |
| W5 | Reference model for apply-delta, merge, land order, graph, blame | `crates/bee-testkit/src/model` | F1 |
| W15 | Git object format: parse and serialize commits, trees, tags; rebuild exact git ids from bee objects plus the Meta `git` extension; differential tests against `git` | `crates/bee-git/src/object` | F1 |

### Wave 2: parallel (start once W1 lands)

| # | Task | Owns | Depends on |
|---|---|---|---|
| W6 | Postgres + SQLite metadata store passing the store conformance suite | `crates/bee-store-sql` | W1 |
| W7 | S3 + disk blob store, with hot/cold storage tiers (SPEC §6.5) and signed CDN URLs (SPEC §10.5) | `crates/bee-store-blob` | W1 |
| W8 | Graph, ancestry, merge base, log, path history; eager derived types | `crates/bee-history`, `crates/bee-derived` | W1, W5 |
| W9 | Git history importer with exact-id round trip (SPEC §17.4) and fidelity tests on real repos | `crates/bee-import`, `corpus/` | W1, W15 |
| W10 | Working copy: sparse profile, checkout, status, commit, journal | `crates/bee-wc` | F2, W1 |

### Wave 3: integration

| # | Task | Owns | Depends on |
|---|---|---|---|
| W11 | Server write path: upload, CreateChangeset, rebase, merge, land, refs; the required land coalescer with one fenced owner per protected ref and block-allocated event sequences (SPEC §7.4, §11.7); read-your-writes via `min_version` (SPEC §6.3); signed-URL object reads (SPEC §10.5); `--dev` mode; deadlines, priority tiers and per-delegation-root quotas (SPEC §15.3–15.4) | `crates/bee-server` | W2, W4, W6, W7, W8 |
| W12 | CLI with `--json` and schemas | `crates/bee-cli`, `schemas/v1` | W10, W11 |
| W13 | MCP server (`bee mcp` and `/mcp`) | `crates/bee-mcp` | F2, W11 |
| W14 | Conformance and load: linearizability (including coalescer owner failover and fencing), complexity gates, git differentials, many-agent simulation, and the load harness that measures every SPEC §16 target with 1,000 concurrent landers on one ref; `docs/benchmarks.md` | `crates/bee-conformance`, `docs/benchmarks.md` | W11 |
| W16 | Git smart HTTP: upload-pack (clone, fetch, shallow, `blob:none`), receive-pack mapped to workspace updates and Land, pack cache | `crates/bee-git-http` (layer 6; `bee-git` at layer 2 cannot reach the store or server) | W11, W15 |
| W17 | `bee import github` mirror phase: GitHub App auth, git-first import, org migration archive, API backfill with checkpoints, placeholder principals and claims, webhook live sync, fidelity report, `import-report` schema | `crates/bee-github`, `schemas/v1/import-report.schema.json` | W9, W11 |

## Gate before production

| # | Task | Owns | Depends on |
|---|---|---|---|
| G1 | Repeat the metadata benchmark (`bench/db`: hot ref, ingest, reads, plus fault and operations runs) on at least three machines in separate zones with synchronous replication, and record whether SPEC §16's land targets hold with the coalescer (SPEC §16, production gate) | `bench/db`, `docs/benchmarks.md` | W11, W14 |

The cloud does not serve customers until G1 passes. A failure reopens the storage decision and
SPEC Appendix A item 8 (path-partitioned landing).

**Exit test for phase 1:**
1. `bee import github terramind-org/bee` into a `bee-server --dev` instance. Every commit
   keeps its git id, and the fidelity report has no unexplained drops.
2. Make a change with an agent token through `bee mcp` and land it.
3. Run `git clone` against Beehive and `git push` a second change, which lands under policy.
4. Verify `log`, `diff` and `blame` over the API.
5. Show the load harness report for SPEC §16, including which targets were missed and by how much.

## Phase H: Beehive web (discovery and editions)

Redesign `apps/web` into a forge with two editions (cloud and enterprise) from one codebase: real
`/[owner]/[repo]` URLs, public discovery, anonymous read of public repos, cloud sign-up and pricing,
an admin console, Docker images and agent prompts. Screens and behaviour: `docs/spec/web-ui.md`;
patterns: `docs/design/tokens.md`. Builds on the Nucleus redesign (the web app is named Beehive).
Same rules as above: **Owns** lists the only paths a task may edit, and every task ships its tests.

| # | Task | Owns | Depends on |
|---|---|---|---|
| H0 | docs-spec: amend the web UI spec, plan and tokens | `docs/spec/web-ui.md`, `docs/PLAN.md`, `docs/design/tokens.md`, `docs/PRODUCT.md` (cross-references only) | none |
| H1 | anon-read: serve public repos to anonymous callers, `allow_anonymous_public_read`, `ListLeaderboard` | `crates/bee-server/src/access.rs`, `crates/bee-server/src/services/search.rs`, `proto/bee/v1/search.proto`, UserService read paths in `crates/bee-forge/src/community/svc/users.rs`, `[forge]` key in `config.rs`, `crates/bee-conformance/tests/public_read.rs`, generated `search_pb.ts` and schemas | none |
| H2a | foundation-routes: move screens under owner/repo URLs | `apps/web/app/**` route moves and layouts, `middleware.ts`, `lib/routes.ts`, `lib/nav.ts`, `lib/api/server.ts`, `components/{shell,public-header,repo-header,repo-picker,command-palette,repo-card,heatmap,star-button,follow-button}.tsx`, existing e2e specs (path updates) | H9 |
| H2b | foundation-data: community, discovery, cloud and admin data methods | `apps/web/lib/api/{methods,types,client,contract.test}.ts`, `lib/api/ops/**`, `lib/api/fixture/**`, `lib/copyas.ts`, `e2e/seed.ts` | H1, H9 |
| H3 | home: signed-in dashboard and signed-out instance landing | `app/(public)/page.tsx`, `app/(public)/loading.tsx`, `components/home/*`, `e2e/home.spec.ts` | H2a, H2b |
| H4 | repos: repository list, repo home, profiles | `app/(public)/repositories/*`, `app/(public)/[owner]/page.tsx`, `app/(repo)/[owner]/[repo]/page.tsx` and loading files, `components/repo/*`, `e2e/repos.spec.ts` | H2a, H2b, H8 |
| H5 | discover: explore, trending, topics, leaderboards, Atom feeds | `app/(public)/{explore,trending,topics,leaderboards}/*`, both `feed.atom/route.ts`, `components/discover/*`, `lib/atom.ts`, `e2e/discover.spec.ts` | H2a, H2b |
| H6 | infra-ui: surface recent infra changes in the UI | `app/(repo)/[owner]/[repo]/changes/[id]/*`, `components/{change-actions,domain,degraded-banner}.tsx`, `app/(repo)/[owner]/[repo]/releases/*`, `e2e/changes.spec.ts` additions | H2a, H2b |
| H7 | docker: images, compose files, self-hosting guide | `Dockerfile`, `apps/web/Dockerfile`, `.dockerignore`, `deploy/**`, `docs/self-hosting.md`, `scripts/check-docker.sh`, `apps/web/next.config.ts` (`output: "standalone"` only) | none |
| H8 | prompts: agent prompts, guide and `llms.txt` | `docs/prompts/*`, `docs/agents-guide.md`, `lib/prompts.ts`, `components/prompt-card.tsx`, `app/(public)/prompts/*`, `app/llms.txt/route.ts`, `e2e/prompts.spec.ts` | H2a |
| H9 | edition: `ServerInfo.edition` and `[server] edition` | `proto/bee/v1/vcs.proto`, `crates/bee-server/src/config.rs`, `crates/bee-server/src/services/object.rs` (ServerInfo), `crates/bee-server/src/ops*` (production check), `docs/spec/server.md`, generated web code for `vcs_pb.ts` | none |
| H10 | cloud: landing, sign-up, onboarding, pricing, org billing | `components/marketing/*`, `components/onboarding/*`, `app/(public)/pricing/*`, `app/(public)/new/*`, `app/signin/*`, `app/(shell)/orgs/[org]/settings/billing/*`, `e2e/cloud.spec.ts` | H3, H2b |
| H11 | admin: enterprise admin console | `app/(shell)/admin/*`, `components/admin/*`, `e2e/admin.spec.ts` | H2a, H2b |

H1, H7 and H9 can start together; H8 starts once H2a lands. A final judge pass owns only fixes where tasks
meet and `scripts/check-web-e2e.sh` (the edition matrix, web-ui.md §12).

**Exit test for phase H:**
1. `scripts/check.sh` and `scripts/check-web-e2e.sh` pass for both editions.
2. A signed-out visitor reads a public repo, trending, a leaderboard and an Atom feed; a private repo
   answers the 404 page.
3. `docker compose -f deploy/compose.dev.yml up` serves a working server and web app.

## Phase B: change requests as briefs with media evidence

Change requests read as briefs for people supervising agents: words first (outcome, what needs
you, risk), media evidence (screenshots, before/after, video, benchmarks, test reports) labelled
"From the agent" or "Verified by CI" and flagged stale after a new revision, an advisory checklist
of what the author still owes, and code on demand. Contract: SPEC §11.9 and
`proto/bee/v1/briefs.proto`; screens: `docs/spec/web-ui.md`; patterns:
`docs/design/tokens.md`. The checklist nudges and never gates landing. Same rules as above: **Owns**
lists the only paths a task may edit, every task ships its tests and its own coverage shard
(`docs/coverage/tests/brief-<task>.csv`), and no task adds a runtime dependency (native `<img>` and
`<video>`, inline SVG charts, a hand-written PNG encoder for seed media).

| # | Task | Owns | Depends on |
|---|---|---|---|
| B1 | contract: SPEC §11.9, §11.7 events, §12.3 row; this table; PRODUCT `AS-CR-04` and rows F413-043..046, F414-037, F424-006..008; web-ui and tokens; `briefs.proto` and the `forge.proto` brief fields; regenerated schemas, CLI schemas and web bindings; `BriefService` on the `UNREGISTERED` allow-list | `SPEC.md`, `docs/{PLAN,PRODUCT}.md`, `docs/spec/web-ui.md`, `docs/design/tokens.md`, `docs/coverage/features.csv`, `docs/contracts/amendments.md`, `proto/`, `schemas/`, `apps/web/lib/gen/` | the posts protos |
| B3 | forge: `Changes::set_brief` and `OpenChange.brief`; the `briefs/` module (store, transfer, media sniffing and image sizes, `checklist.rs`, service, maintain loop); tests `briefs_brief.rs`, `briefs_evidence.rs` on memory and SQLite | `crates/bee-forge/src/{changes,briefs}/**`, `lib.rs`, `module.rs`, `docs/spec/forge-{changes,briefs,core}.md` | B1 |
| B4 | server: construct and register `Briefs`, the `/_evidence/*` route with re-authorisation, the maintain loop, remove the allow-list entry, `tests/briefs.rs` (at most 4 tests), conformance fixture `brief.json`, regenerated `verification.md` | `crates/bee-server/src/forge/**` and its tests, `crates/bee-conformance/fixtures/brief.json`, `docs/spec/{server,verification}.md` | B3 |
| B5 | CLI, MCP and prompts: `bee cr brief`, `cr checklist`, `cr evidence add\|list\|rm`, brief flags on `cr open`; `brief_*` MCP tools in `core` and `changes`; `docs/prompts/show-your-work.md` and its links | `crates/bee-cli/src/wc/{aliases,evidence}.rs` and tests, CLI schemas, `crates/bee-mcp/src/{client,catalog}.rs`, `docs/prompts/**`, `docs/agents-guide.md`, `apps/web/lib/{prompts,prompts.gen,llms}.ts` and tests | B4 |
| B6 | web data: view types and methods (`BriefService`, `ListComments`, `CommentChange`, `GetSessionsForChange`), `ops/briefs.ts`, the fixture adapter with generated media, the same-origin evidence route, Copy as forms | `apps/web/lib/api/**`, `apps/web/lib/media/**`, `apps/web/lib/copyas.ts`, `apps/web/app/api/evidence/[...path]/route.ts` | B1, web-data |
| B7 | web blocks: pure libraries (`whatchanged`, `status`, `asks`, `checkwords`, `team`, `hunks`, `evidence-view`, `changemd`, `queue`, `runsummary`; vitest, no clock); `components/brief/**`; Markdown `evidence:` images; `change-actions.tsx` | those paths | B6 |
| B8 | seed and media: deterministic PNG before/after frames, a committed webm of at most 400 KB with captions, seeded CRs `ui`, `cache`, `fix`, `rotate` and a docs change that owes nothing | `apps/web/e2e/seed*`, `apps/web/e2e/helpers.ts`, `apps/web/scripts/evidence-media.mjs` | B4, B6 |
| B9 | change detail: Brief, Code, Checks and For agents tabs, `change.md`, `?rev`/`?since` to Code; the post panel and the "Inspired by" banner in Brief; e2e `brief.spec.ts`, updated `changes.spec.ts`, axe on every tab, Lighthouse on Brief | `apps/web/app/(repo)/[owner]/[repo]/changes/[id]/**`, `components/diff-viewer.tsx`, those e2e files | B7, B8, R1, web-compose |
| B10 | queue and runs: the review-queue change list and the run "What happened" summary; `queue.spec.ts`, `runs.spec.ts` additions | `changes/{page,loading}.tsx`, `runs/[id]/**`, `components/queue/**` | B7, B8, R1 |

B3 and B6 start together once B1 lands; B4 then unblocks B5 and B8; B9 and B10 run in parallel at
the end. Critical path: B1 → B3 → B4 → B8 → B9 → judge. The judge owns only fixes where tasks meet,
checks SPEC against code, and flips the new coverage rows to OK.

**Exit test for phase B:**
1. `scripts/check.sh` and `scripts/check-web-e2e.sh` pass for both editions; Lighthouse
   performance is at least 0.90 on the Brief tab of the seeded `fix` and `ui` changes.
2. `AS-CR-04` passes by hand against `bee-server --dev` and the web app.

## Beehive agent-first redesign

The Agents tab reads repository work through `SwarmService`. Scope is PRODUCT §4.19 and
the contract is SPEC §11.10.

| Task | Owns | Depends on |
|---|---|---|
| swarms-server | `proto/bee/v1/swarms.proto`, `crates/bee-forge/src/swarms/`, run lifecycle integration, policy action and service registration, swarm forge and conformance tests and fixtures, swarm web API operations and fixture, generated bindings and schemas, SPEC and agent contract docs, coverage | posts-green |
| agents-tab | Repository Agents screens and agent-facing facts | swarms-server |
| safety-server | `proto/bee/v1/safety.proto`, forge safety module, focused run/swarm and merge hooks, service and tool registration, conformance, web operations and fixture, and binding safety docs | swarms-server |
