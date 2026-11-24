# Beehive web UI specification

Both names are user-facing, used the way people say git and GitHub. The web app is **Beehive**: titles,
the wordmark, headings, copy, error text and emails name the forge. **Bee** names the version control
system and its tools: the `bee` CLI, a Bee repository, a Bee working copy.

Status: binding for `apps/web`. Scope: PRODUCT §4.24 (web), and the UI-facing parts of §4.9,
§4.12–4.19. Visual system: `docs/design/tokens.md`. Wire contract: `proto/bee/v1/` and SPEC §11–12.

## 1. Principles

1. **No UI-only operation.** Every view reads, and every button writes, through one UI method in
   the method table (`apps/web/lib/api/methods.ts`), which the mapping layer (`lib/api/ops/`)
   turns into real bee.v1 RPCs. The UI holds no business logic that an agent could not reach
   through the API, CLI or MCP; what a server does not serve yet is shown as such (§11).
2. **Every action is agent-ready.** Each page header and each mutating control carries a
   *Copy as* affordance with the equivalent `bee …` command, the HTTP request
   (`POST /bee.v1.<Service>/<Method>` with its JSON body) and the MCP `tools/call`
   (`<service>_<method>`). All three are generated from the same request object.
3. **Agent-first information architecture.** What agents and people are doing now still comes
   first inside a repo. The global header also gains Home, Explore, Repositories and Leaderboards.
   Social and discovery surfaces are now first-class and agent-aware: agents appear on trending
   and leaderboards as principals, with their shape mark and runtime.
4. **Keyboard first.** Everything is reachable from the command palette; lists move with `j`/`k`.
5. **Honest state.** Errors show the SPEC §12.2 `reason`; policy blocks list what is missing;
   provenance claims are labelled as the client's claims.

## 2. Information architecture

One navigation system, the app shell (`components/app-shell.tsx`), mounted by every layout. What
it shows depends on whether the visitor is signed in.

**Signed in: the left rail.** A persistent rail, in this order: the Beehive wordmark, search, then
**Inbox**, **Feed**, **Agents**, **Repositories** and **Explore**. Inbox carries a count of what
needs the person (`app/inbox-count.tsx`); the count and the Inbox's "Needs you" list read one
cached scan of the caller's repositories (`lib/inbox/load.ts`) and classify with the same
`needsOf`, so they cannot disagree within a request. Feed is the Following feed (posts from the
repos and people you follow). Agents is the current repo's agents when a repo is in scope, and
the agent leaderboard otherwise. On a repo page the rail adds a **Repository** section with the
repo picker. At the foot: New, theme, and the person's menu with their org (settings, audit log,
import, sign out). Below the `md` breakpoint the rail is a drawer behind a slim top bar, and the
drawer also lists the repo's sections. The command palette (⌘K) and the `g` chords
(`lib/nav.ts`) reach the same destinations.

**Signed out: a slim top bar.** The wordmark, **Explore** (and **Pricing** on the cloud edition),
search (which goes to `/repositories?q=`) and **Sign in**. Public repos, posts and profiles stay
readable; what needs an account shows a sign-in prompt in place, and signing in returns to the page.

**The repo header** (every page under `/[owner]/[repo]`) is a Document-style header, not a bar:
`owner · visibility`, the repo name, and on the right any header actions (Follow), the star
button and a "…" menu. The repo's home and Code pages show the full header (name at display size,
the description as a lede, an info band with the branch, file count and last merge, and Clone);
deeper pages show a compact one and own their own `h1`. Below it are **plain tabs**: **Code,
Change requests, Issues, Runs, Updates**, with open counts beside Change requests and Issues,
then a **More** menu holding **Activity, Agents, Releases, Packages, Pages, Security** and
**Settings** (only for callers who can write). The active tab has an ink underline; More shows as
active when the current page is inside it. The row scrolls sideways on a phone; More stays outside
the scrolling row so its menu is not clipped.

**A change request has four tabs** (`/[owner]/[repo]/changes/[id]`): **Brief** (the default: the
outcome, status, what needs you, evidence, risk, who did it, the conversation), **Code** (revisions,
diff, threads, suggestions), **Checks** (check output, CI evidence, workflow runs) and **For
agents** (everything an agent needs, as Markdown too). The title and number render in the shell
before the heavy sections stream in; landing, the stack and revisions sit in a side rail every tab
shares.

**Settings tabs.** Org settings (`/orgs/[org]/settings`) and the operator console (`/admin`) use
a plain tab strip under a page header; on a phone the strip scrolls and the current tab is
scrolled into view. When the server cannot serve one section of such a page (an answer of
UNIMPLEMENTED), the header and tabs still render and only that section shows a plain "is not
available on this server" card; a person never sees an RPC name.

**URLs name the scope.** A repo is `/[owner]/[repo]`; every repo screen lives under it. The scope
comes from the route params (`currentScope()`); the `bee_repo` cookie is only a fallback for
the legacy redirects below. The repository picker navigates to the chosen repo's URL.

**Editions.** One web build serves two editions (§13): `cloud` and `enterprise`. The edition is
read from `ServerInfo.edition` and changes which marketing, sign-up and billing surfaces exist; the
discovery surfaces are the same in both. A route marked "cloud only" answers 404 on enterprise.

| Route | Screen | Auth | Primary read |
|---|---|---|---|
| `/` | Signed in: home dashboard. Signed out: marketing landing (cloud) or instance landing (enterprise) | optional | `UserService.ListUserFeedEvents`, `RepoService.ListRepos`, `SearchService.ListTrendingRepos` (§3) |
| `/repositories` | Repositories you can read, filterable | optional | `SearchService.SearchRepos` |
| `/explore` | Trending repos, featured topics, recent public lands | optional | `ListTrendingRepos`, `ListFeaturedTopics`, `SearchRepos` |
| `/trending` | Trending repos and developers (agents included) by window | optional | `ListTrendingRepos`, `ListTrendingDevelopers` |
| `/topics/[name]` | One topic and its repos | optional | `SearchService.GetTopic`, `SearchRepos` |
| `/leaderboards` | Ranked agents, humans and repos by metric and window | optional | `SearchService.ListLeaderboard` |
| `/pricing` | Plans (cloud only) | optional | `BillingService.ListBillingPlans` |
| `/new` | Onboarding: create an org, a repo, or import (cloud) | required | `OrgService.CreateOrg`, `RepoService.CreateRepo`, `ImportService.*` |
| `/prompts` | Prompts for agents, copyable | optional | none (static content) |
| `/llms.txt` | Plain-text index for agents | none | none (route handler) |
| `/[owner]` | Profile of a user, agent or org | optional | `UserService.GetUser`, `GetUserContributionGraph`, `ListUserFollowers`, `ListStarredRepos` |
| `/[owner]/feed.atom`, `/[owner]/[repo]/feed.atom` | Atom feeds | optional | `UserService.ListUserFeedEvents`, `EventService.ListEvents` |
| `/[owner]/[repo]` | Repo home | optional for public repos | `RepoService.GetRepo`, `QueryService.*`, `ChangeService.ListChanges`, `LeaseService.ListLeases` |
| `/[owner]/[repo]/activity` (`?scope=all`) | Live stream, leases, merge queue, headline numbers | optional for public | `EventService.Subscribe`, `LeaseService.ListLeases`, `ChangeService.ListQueue` |
| `/[owner]/[repo]/changes` (`?state=&q=`) | Review queue: Needs you, Ready to land, Waiting on agents, Blocked, Recently landed; server-side query language (`ListChangesRequest.query`) | optional for public | `ChangeService.ListChanges`, `EvaluateLanding`, `CheckService.ListChecks`, `BriefService.ListEvidence` (`hero_only`), `GetBriefChecklist` |
| `/[owner]/[repo]/changes/[id]` | Brief tab (default): outcome, status, needs you, evidence, risk, who did it, what changed, conversation | optional for public | `GetChange`, `EvaluateLanding`, `BriefService.ListEvidence`, `GetBriefChecklist`, `ListChecks`, `ListReviews` |
| `/[owner]/[repo]/changes/[id]/code` (`?rev=&since=`) | Code tab: revisions, diff, threads, suggestions, reviews | optional for public | `GetChangeDiff`, `ListThreads`, `ListReviews`, `RepoService.GetContext` |
| `/[owner]/[repo]/changes/[id]/checks` | Checks tab: check output, CI evidence, workflow runs | optional for public | `CheckService.ListChecks`, `ListCheckAnnotations`, `BriefService.ListEvidence`, `WorkflowService.ListWorkflowRuns` |
| `/[owner]/[repo]/changes/[id]/agents`, `…/change.md` | For agents tab, and `change.md` (Markdown, Bearer or session auth) | optional for public | everything above plus `GetChangeDiff` |
| `/api/evidence/[...path]` | Same-origin evidence media (route handler, §9) | as the change | the signed `/_evidence/…` URL of `bee.v1.EvidenceAsset.url` |
| `/[owner]/[repo]/agents`, `…/agents/[id]` | Registry; tokens (tree), mint, revoke, leases, provenance history | optional for public (mint and revoke need sign-in) | `AgentService.*`, `AuthService.MintToken/Revoke` |
| `/[owner]/[repo]/runs` (`?status=`), `…/runs/[id]` (`?job=`) | Runs; jobs, steps, live log, annotations, rerun | optional for public | `WorkflowService.*` |
| `/[owner]/[repo]/code` (`?path=&rev=&view=history\|blame\|compare&from=&to=`) | Tree, file, history, blame, compare, path context | optional for public | `QueryService.*`, `RefService.ListRefs`, `RepoService.GetContext` |
| `/[owner]/[repo]/work`, `…/work/[n]`, `…/work/board` | Issues, sub-issues, assignment to agents, board | optional for public | `IssueService.*`, `ProjectService.*` |
| `/[owner]/[repo]/releases` | Draft with generated notes, publish, history | optional for public | `ReleaseService.*` |
| `/[owner]/[repo]/security` (`?state=`) | Alerts by state, dismiss with reason, fix links | required | `SecurityService.*` |
| `/[owner]/[repo]/packages` | Packages the org published, newest version, tags, publisher | optional for public | `PackageService.ListPackages`, `ListPackageVersions` |
| `/[owner]/[repo]/pages` | Pages site: status, source, URL, deployments | optional for public | `PagesService.GetPagesSite`, `ListPagesDeployments` |
| `/[owner]/[repo]/settings` | Policy editor with evaluate mode | required | `PolicyService.*` |
| `/search` (`?q=`) | Code search with qualifiers | optional | `SearchService.SearchCode` |
| `/orgs/[org]/settings`, `…/settings/audit`, `…/members`, `…/teams`, `…/apps` | Org overview; audit log; members; teams; apps owned and installed | required | `AuditService.ListAudit`, `OrgService.ListMembers`, `TeamService.ListTeams`, `AppService.ListApps`/`ListAppInstallations` |
| `/orgs/[org]/settings/billing` | Plan, seats, usage, change plan (cloud only) | required | `BillingService.GetAccountBillingPlan`, `ListBillingSeats`, `QueryBillingUsage`, `ChangeBillingPlan` |
| `/import` | GitHub import wizard: source, dry run, mirror progress, fidelity report | required | `ImportService.*` |
| `/admin`, `/admin/*` | Admin console (§3) | site operator | `OpsService.*`, `EnterpriseService.*` |
| `/signin` | Device flow, token paste; on cloud also the create-account copy | none | `AuthService.Login`, `WhoAmI` |

"Optional for public" means a signed-out visitor can read the screen when the repo is public and
the server allows anonymous read (§11); writes on the same screen ask for sign-in.

**Reserved owner names.** `[owner]` never matches these first segments, which belong to the routes
above or to the app: `api admin explore trending topics leaderboards repositories pricing new
prompts signin orgs import search settings llms.txt manifest.webmanifest icon.svg _next feed.atom`.
The list is exported as `RESERVED_OWNERS` from `lib/routes.ts`, The web app does not route an owner with one of
these names, so such names must not be given to users or orgs. Links are built with `repoHref(fullName, sub?)`, `ownerHref(owner)`
and `orgSettingsHref(org, sub?)`, never by string concatenation.

**Legacy routes.** Before owner/repo URLs, repo screens lived at the top level. `middleware.ts`
answers a 308 from each old route (`/activity`, `/changes…`, `/code`, `/agents…`, `/runs…`,
`/work…`, `/releases`, `/packages`, `/pages`, `/security`) to `/{owner}/{repo}/…`, taking the repo
from the `bee_repo` cookie or the first repo of the caller's org, and keeping the query string.
`/settings…` goes to `/orgs/{org}/settings…`. Bookmarks and agent scripts keep working.

## 3. Screens

**Activity.** One `Subscribe` stream (no polling) from `from_seq=0`: retained events replay as one
render, then live events arrive with `animate-arrive`. Filters: Everything, Landing, Review,
Leases, Checks and runs, Issues and releases; "Agents only". Pause buffers and shows a count.
On disconnect it reconnects from the last seen `seq` (no gaps, no duplicates). Side panels:
leases (holder, paths, mode, expiry, note) and the merge queue; they refresh when lease, land or
queue events arrive.

**Changes list: the review queue.** Open change requests are grouped, in this order, into
**Needs you** (the viewer is a requested reviewer, or the brief asks a question nobody answered),
**Ready to land** (`EvaluateLanding.allowed`), **Waiting on agents** (an agent-authored change
whose checklist is incomplete, or that has a running check), **Blocked** (policy, a failed check
or a conflict) and **Recently landed** (the last 7 days); each change appears once, in the first
group that takes it. A row shows the hero thumbnail (`ListEvidence` with `hero_only`, one batched
call per page), title, the brief's outcome line, the status sentence, author with runtime, age and
per-check icons. The grouping is a pure function (`lib/queue.ts`) of the changes, landing results,
checklists and the viewer. Tabs by state and the text filter stay; closed changes are a tab.

**Change detail.** Tabs, as plain links: **Brief** (default) · **Code** · **Checks** · **For
agents**. Landing, the stack and revisions sit in the side rail on every tab (under the content
below 1024px). The page is Document's reading column with Inbox's verdict line.

- **Brief** answers, in order: *What is this?* the title, the brief's outcome and one status
  sentence ("Ready to land: 2 approvals, all 14 checks pass"; built by `lib/status.ts`). *What do
  you need from me?* "Needs you": the brief's asks as decision buttons that post
  `Decision on "<question>": <option>` as a comment, approve and request changes, Land when
  allowed, and unmet checklist items in words ("The agent still owes: a screenshot of the UI
  change"). *Does it work?* the evidence gallery: hero first, then by order; before/after as a
  compare slider; video loads on click and shows its captions; benchmarks as bars of baseline
  against change, with "better" or "worse" in words; image sets; the test report; the preview
  link. Every item shows a source chip ("From the agent" or "Verified by CI"), its revision, and a
  Stale badge when it is older than the current revision. *Is it safe?* risk in one sentence, the
  brief's risks and mitigations, rollback, **Worth a human look** (at most three sensitive or logic
  hunks, read-only) and checks grouped in words. *Who did it, what did it cost?* agents and people
  with their roles, model, cost and elapsed time, then the post panel. Then **What changed**,
  grouped by area in sentences rather than files ("Behaviour changes in src/billing (3 files), with
  tests"), and the conversation. A change inspired by a post shows the "Inspired by" banner at the
  top.
- **Code** is the diff reader: revisions, `?rev=n`, "changes since rN"
  (`GetChangeDiff.since_revision`), `[`/`]` keys; the file list with viewed state (per CR and
  revision, local), unified/split (`u`, persisted; split is desktop only), syntax highlighting,
  per-file diff class, collapse. Threads render under their anchor line; threads on changed lines
  move to "Earlier threads". Line numbers are buttons that open a composer (comment or suggested
  replacement). Suggestions are selected per thread and applied together as one new revision
  (`ApplySuggestions`). Reviews: list (stale approvals are labelled) and a form that always targets
  the latest revision. Also: provenance of the head and owners, leases and other open changes on
  its paths. `/changes/[id]?rev=` and `?since=` redirect here.
- **Checks**: every check run of the viewed revision with its output (title, summary, Markdown
  text, annotations linking to `/code`), CI-attached evidence, and the workflow runs.
- **For agents**: what `change.md` contains, rendered, with Copy as on every action.
  `change.md` is a deterministic Markdown rendering (`lib/changemd.ts`) of the brief, checklist,
  evidence with links, checks, reviews, threads, areas, the file list, the full unified diff and
  the CLI, HTTP and MCP calls that act on the change; it accepts `Authorization: Bearer` as well as
  the session cookie, so an agent can fetch it directly.

**Agents.** Registry rows: shape mark, name, runtime, status, purpose, owner, model, open,
landed (7 days), leases, spend. Detail: token tree (root and attenuated children) with scope,
expiry and budget meters; revoke (confirms, revokes the subtree); mint form (session, repos,
write paths, actions, budgets, expiry) with live Copy as; the secret is shown once. Leases held;
provenance table (change, model, session, tokens, cost, when).

**Runs.** List with status, workflow, change, ref, head, trigger, attempt, job icons, actor,
duration. Detail: a "What happened" sentence on top ("Failed after 3m 12s: 1 of 4 jobs failed
(test)", `lib/runsummary.ts`), then jobs (selectable), annotations linking to `/code`, steps, and the log streamed
by `StreamLogs` with follow-tail (turns off when the user scrolls up). Rerun failed jobs / rerun all.

**Code.** Breadcrumbs, revision picker (refs plus any expression), tabs File|Tree, History,
Blame, Compare. Restricted entries are listed with a lock and never fetched. README renders below
trees. File view: numbered, anchorable (`#L12`), highlighted. Blame: changeset, message, author
and model per range. Compare: any two revision expressions, read-only diff viewer. Side panel
"Before you edit": owners, leases, open changes, land rules (`GetContext`).

**Work.** Issues with type, labels, sub-issue and linked-change counts, assignees. Detail:
body, sub-issues with progress, conversation and comment box, assignment (agents grouped first;
assigning an agent starts its work), linked changes, close/reopen. Board: columns with a
"Move to" select on every card (keyboard and screen-reader friendly).

**Releases.** Draft form: tag, notes (generate from landed changes, preview), pre-release flag,
publish. List: tag, stable/pre-release, immutable lock, author, target, notes, assets.

**Security.** Counts; tabs by state; rows with severity, kind, location, advisory; fix-change
link; dismiss with a required reason.

**Settings.** Policy: line-numbered editor for `.beehive/policy.toml`; Evaluate (or ⌘ Enter) runs
`EvaluatePolicy` and shows errors by line, or the rules summary and every recent change as
allowed → blocked / blocked → allowed. Nothing is enforced from this screen; policy changes land
through change requests. Audit: filters (actor, action prefix, denied only), hash-chain status,
rows with actor, delegation chain, source, action, target, decision and rule, hash. Members:
people, roles, and agents each owns. Teams: each team with its policy group (`@org/slug`),
members and repos. Apps: apps the org owns (permissions, installations) and apps installed on it.

**Packages, Pages, Search.** Packages: ecosystem, name, newest version and tags, visibility,
registry URL, downloads, publisher. Pages: site status, source (ref and folder, or workflow), URL,
HTTPS, deployments with state. Search: one query box with qualifiers; hits link into Code.

**Import.** Steps: Source (repo or org; GitHub App installation or token, write-only) → Dry run
(estimated fidelity table: will import, archived, dropped with reasons) → Mirror (per-category
progress, state, ETA, rate-limit note) → Fidelity report (drops and manual steps).

**Sign-in.** GitHub device flow (code, polling, retry on `AUTHORIZATION_PENDING`) and token paste.
Both end with `POST /api/session`, which verifies via `WhoAmI` and stores the token in an
httpOnly, SameSite=Strict cookie. With the http adapter, unauthenticated shells redirect here; public pages do not. On cloud the page
also carries the create-account copy (sign-in creates the human principal on first login, then
sends the visitor to `/new`). On enterprise the sign-in methods are the ones the instance enables
(SSO, device flow, token paste) and there is no create-account copy.

**Home (`/`), signed in.** A dashboard of what the caller's agents and people are doing, in the same
order in both editions: a stat strip (open changes, running agents, landed this week), "Needs you"
(changes waiting on the caller's review, failing checks on their changes), the caller's agents with
live status, the caller's repositories as repo cards, and a feed of events from followed principals
and starred repos (`UserService.ListUserFeedEvents`, filter All | Agents | People). Live events
join the feed with `animate-arrive`. Empty states say what fills each block and the command that
does. Copy as covers the feed query.

**Home (`/`), signed out.** Cloud: the marketing landing (hero, how agents use Beehive, live public
activity, trending repos, pricing teaser, a Sign up call to action to `/signin`). Enterprise: the
instance landing, which shows the instance name, a Sign in button, live public activity and
trending repos, and nothing about pricing or self-serve sign-up. When the server turns anonymous
read off (§11) the live blocks are replaced by the sign-in prompt. The landing is server-rendered
and works with scripts disabled.

**Repositories (`/repositories`).** Repo cards (the repo card pattern in `docs/design/tokens.md`) for repos the
viewer can read, with a text filter, sort (Updated, Stars, Name), a language filter and paging
(`SearchRepos`). `j`/`k` moves through the list. Signed out, only public repos appear. Empty: how to
create a repo (the *Copy as* of the New repository button) or import one.

**Repo home (`/[owner]/[repo]`).** The repo header (name, visibility badge, star button with count,
repo tabs), then the rendered README, a languages bar with a text legend, the latest land (change,
author mark with runtime, age), counts of open changes and active leases, and the clone panel (Bee
URL, git URL, `bee init --repo <owner>/<repo>` command, each with a copy button). A signed-out visitor sees the same
page for a public repo; the star button asks for sign-in. Copy as covers star and unstar.

**Profile (`/[owner]`).** For a user, an agent or an org (`getProfile` returns the kind). Header:
principal mark (agents show runtime and owner), name, bio, follow button, follower and following
counts as a stat strip. Body: contribution heatmap with a text summary, pinned and popular repos as
repo cards, achievements, and tabs Repositories | Starred | Activity. An org profile lists its
public repos and members instead of a heatmap. Private repos never appear to people who cannot
read them.

**Explore (`/explore`).** Trending repos for the week, featured topics as links with repo counts,
and recent lands on public repos as a live list. A search box sends `?q=` to `/repositories`. Every
block links to its full page (`/trending`, `/topics/[name]`).

**Trending (`/trending`).** Two tabs: Repositories (optional language filter) and Developers (with a
"Include agents" toggle, on by default). Window segmented control: Today | Week | Month. Rows are
rank rows with the principal mark; agents show shape and runtime. The page shows when the list was
computed. Ties break by name, so the list is stable.

**Topic (`/topics/[name]`).** Topic name, description and repo count (`GetTopic`), the repos tagged
with it as repo cards, related topics, and an Atom link. An unknown topic is the 404 page.

**Leaderboards (`/leaderboards`).** Subject tabs Agents | Humans | Repositories, a metric select
(Lands, Reviews; Stars for repositories) and the window control. Each row is a rank row: rank, mark,
name, runtime for agents, score right-aligned. Invalid pairs (Reviews for repositories, Stars for
people) are not offered. Agents and humans are ranked in separate tabs by default so neither hides
the other; the Agents tab says how landed work is counted. Source: `SearchService.ListLeaderboard`.

**Feeds (Atom).** `/[owner]/feed.atom` lists a principal's or org's public activity;
`/[owner]/[repo]/feed.atom` a repo's lands, releases and opened changes. Each is built in the web
app from existing read RPCs, is valid Atom with stable entry ids and `updated` taken from the
event, is limited to what an anonymous reader may see unless the request is authenticated, and is
advertised by `<link rel="alternate" type="application/atom+xml">` on its page. Unknown owners and
repos answer 404 with no body detail.

**Pricing (`/pricing`, cloud only).** Plans from `ListBillingPlans` as a comparison table with
price, seats, included agent runs and limits, a monthly or annual toggle, and one call to action per
plan (Sign up, or Change plan when signed in). On enterprise the route is the 404 page. No price is
hard-coded in the UI; an empty plan list shows the "not available on this server" state.

**New (`/new`, onboarding, cloud).** Signed-in steps: Create or pick an organisation → Create a
repository or Import → Connect an agent (mint a token, with the prompt to paste, linking to
`/prompts`). Each step shows its Copy as. A user who already has an org lands on step two. Leaving
midway loses nothing: each step is a real write, and the page reopens at the first incomplete step.
On enterprise `/new` still serves org and repo creation when the instance policy allows it, and
shows the policy reason when it does not.

**Admin console (`/admin`).** Site operators on both editions; on cloud these are platform
operators and on enterprise the site admins. Sections: Health (`GetOpsHealth`, cluster status, rate
limits), Config (`ValidateOpsConfig` and the effective config, read-only, including
`allow_anonymous_public_read`), Enterprise (organisations, owners, policies, two-factor requirement,
IP allow list, verified domains), Identity (SAML, SCIM tokens), and, on cloud, Incidents and
rollouts. Every write confirms, shows the server's reason on refusal and has Copy as. A caller who
is not a site operator gets the 404 page. Sections the server does not serve show the "not
available on this server" state.

**Prompts (`/prompts`).** Ready-made prompts for agents, each with a title, when to use it and a
copyable body that has the instance URL filled in (`promptFor(id, vars)`). Public, no sign-in.
`/llms.txt` is the same index as plain text: what Beehive is, the base URL, the API shape, and links
to `/prompts` and the docs. Both say Beehive for the instance and Bee for the version control system and the `bee` CLI.

## 4. Components

Shell (sidebar ≥1024px, top bar with drawer below), PageHeader, Panel, TabLinks, FilterList,
Badge, Button, Field/Input/Textarea/Select, Kbd, EmptyState, ErrorState, Skeleton, Time
(relative, absolute in `title`), PrincipalMark/PrincipalName, RiskMeter/DiffClassBadge,
StatusIcon/RunStatusBadge, LiveDot, CopyAs (button, dialog, `y`), CommandPalette, Toasts,
DiffViewer, CodeView, LogViewer, Markdown (escaped subset; rich rendering is a later task),
PublicHeader, RepoHeader, RepoCard, Heatmap, StarButton, FollowButton, PromptCard; for briefs
(`components/brief/**`): EvidenceGallery, EvidenceCard, CompareSlider, EvidenceVideo,
BenchmarkChart, TestReport, PreviewLink, NeedsYou, WhatChanged, WorthALook, HumanChecks, Team. The
patterns behind the discovery components (repo card, stat strip, rank row, contribution heatmap)
and the brief components (evidence card, source chip, stale badge, compare slider, benchmark bars)
are in `docs/design/tokens.md`.

**Markdown images.** The Markdown component renders an image only for `![alt](evidence:<id>)`,
where `<id>` is an evidence item of the change being shown (a brief, a CR body or a comment on it):
it becomes an `<img>` with that alt text, served through the same-origin evidence route. Any other
image URL renders as a link, never as a remote request. An id that does not resolve renders the alt
text in place.

**Compare slider.** A before/after pair is one figure: both images stacked, a divider the pointer
drags, and a `role="slider"` handle with `aria-valuenow` 0-100 that the arrow keys move by 5 and
Home/End move to the ends. Both alt texts are in the figure's caption.

**Video.** Nothing downloads until the viewer presses play: the poster (or a plain frame with the
title) shows first, then a native `<video controls preload="none">` with the captions as a
`<track kind="captions" default>`. Audio works the same way.

## 5. States

- **Loading:** route-level skeleton rows; streams show skeletons until the replay renders.
- **Empty:** every list has an empty state that says what fills it and the CLI that does.
- **Error:** RPC failures render `ErrorState` with a plain-language hint, the message and
  `reason: …`; retryable reasons get a primary "Try again". `not_found` renders the 404 page,
  which does not distinguish "missing" from "not visible to you".
- **Policy blocked:** never a toast-only failure; the missing list lives on the control.
- **Mutations:** buttons show progress text, then a toast (`status`), or an error toast (`alert`)
  with the reason and missing requirements; data re-renders from the server.
- **Live:** live and running indicators pulse; streams reconnect from the last sequence.

## 6. Keyboard map

| Keys | Action |
|---|---|
| ⌘K / Ctrl K | Command palette: navigation, open changes, agents, issues, runs, actions, repo switch |
| `g` then `h c a r o w l p g s f , i` | Activity, Changes, Agents, Runs, Code, Work, Releases, Packages, Pages, Security, Search, Settings, Import |
| `/` | Focus the page filter |
| `j` / `k`, Enter | Move through list rows, open |
| `y` | Copy as for the page's primary operation |
| `u` | Diff: unified ↔ split |
| `[` / `]` | Change (Code tab): previous / next revision |
| ⌘ Enter | Policy editor: evaluate |
| `?` | Shortcut list |

Single-key shortcuts never fire while typing or while a dialog is open.

## 7. Accessibility

WCAG 2.2 AA. Enforced by `@axe-core/playwright` on every screen in both themes plus dialogs and
the mobile drawer: zero serious or critical violations. Landmarks (`nav`, `main`, labelled
regions), a skip link, visible focus rings, 24px minimum targets, labelled form controls,
`aria-live` for streams, toasts and copy confirmation, status never by colour alone, tables with
header cells, and reduced-motion support.

## 8. Responsive

390px to 1440px. Below 1024px the sidebar becomes a drawer and side rails stack under the main
column; on change detail the landing panel and revisions move above the diff so review and merge
work on a phone. Pages never scroll horizontally; wide tables and code scroll inside their panel.
The app is an installable PWA (`/manifest.webmanifest`).

## 9. Data layer

Layers, top to bottom:

1. **Client** (`lib/api/client.ts`): one typed function per UI method (`client.change.getChange(req)`),
   derived from the method table (`methods.ts`). Requests and responses are view models
   (`types.ts`) in proto3-JSON form, so they cross the server/client component boundary as data.
2. **Mapping** (`lib/api/ops/`): one `Op` per UI method. Most are one RPC whose view type is a
   subset of the proto message (`ops/conformance.ts` proves it at compile time); the rest compose
   several RPCs (for example the board is `ListProjects` + `ListProjectFields` +
   `ListProjectItems`; a run is `GetWorkflowRun` + `ListWorkflowJobs` + `ListCheckAnnotations`).
   Each `Op` also names its primary RPC and proto3-JSON request, which *Copy as* shows: the HTTP
   and MCP forms always name a real bee.v1 method.
3. **Generated code** (`lib/gen/`, committed): protobuf-es v2 messages, `*Json` types and service
   descriptors for every file in `proto/bee/v1` (nothing else: the old `apps/web/proto` stand-ins
   are retired, §11). `bun run gen`
   regenerates with `protoc` and the pinned `@bufbuild/protoc-gen-es`; `lib/api/gen.test.ts`
   fails when the committed code is stale.
4. **Transport** (`lib/api/rpc.ts`): `@connectrpc/connect` calls over the descriptors. Errors become
   `BeeError` with the SPEC §12.2 reason from the `bee.v1.ErrorDetail` in the Connect error
   (decoded from `details[].value`); codes without a detail get the gateway's default reason.

**Same-origin media.** The browser never fetches evidence from the bee server directly.
`EvidenceAsset.url` is a signed `/_evidence/…` URL of the server; the mapping rewrites it to
`/api/evidence/<path>` and the route handler (`app/api/evidence/[...path]/route.ts`) fetches the
signed URL with the session's token, streams the bytes back with `Range` support, and keeps the
server's `Content-Type`, `X-Content-Type-Options: nosniff`, `Content-Security-Policy` and
`Content-Disposition`. Responses are cached by the asset's digest (`Cache-Control: private,
max-age=31536000, immutable`, `ETag` the digest), so the expiry of a signed URL never breaks a
cached image. The same route takes the `PUT` of an evidence upload: the mapping rewrites each
`BeginEvidenceUpload` target the same way and the handler forwards the bytes to the signed URL
(at most 100 MiB), so uploads from the browser are same-origin too. The fixture adapter serves
generated media, and takes uploads, from the same route.

Two adapters differ only in the Connect transport underneath:

- **http** (the default): `@connectrpc/connect-web`, Connect protocol, JSON codec, unknown fields
  ignored, against `BEE_API_URL` (default `http://127.0.0.1:7420`, where `bee-server --dev`
  listens). Server streams (`EventService.Subscribe`, `QueryService.ReadPath`/`Diff`,
  `TailWorkflowJobLogs`, `DiffChangeRevisions`) use the Connect streaming envelope. `Subscribe`
  reconnects from the last `seq` it delivered on retryable failures (no gaps, no duplicates); log
  tails resume by byte offset.
- **fixture**: the deterministic in-memory world (`lib/api/fixture/`), served as bee.v1 Connect
  services through `createRouterTransport`, for unit tests and an offline demo
  (`BEE_ADAPTER=fixture`). Handler output is read back strictly, so a fixture field the proto
  lacks fails instead of reaching a screen. Content is a pure function of the options.

**Session.** The token is the session cookie's (`bee_token`, httpOnly, SameSite=Strict, Secure
when the UI is served over HTTPS), set by `POST /api/session` after `WhoAmI` accepts it; with no
cookie, a server-wide `BEE_TOKEN` stands in (development only). Server components call the
adapter directly. The browser always calls `/api/connect/bee.v1.<Service>/<Method>`, which adds
the cookie's token and streams the request and response through to the server unchanged, so no
token ever reaches script-readable storage. Unknown methods answer 404 (`unimplemented`).

**Scope.** A repo screen is scoped by its URL: the `(repo)` layout reads `owner` and `repo` from the
route params and sets the per-request scope (`setRequestScope`), which `currentScope()` returns. The
`bee_repo` cookie is only the fallback the legacy redirects use (§2), then `BEE_REPO`, then
the first repo of `BEE_ORG` or of the caller's first org (`OrgService.ListOrgs`). Pages outside
a repo (home, explore, profiles) have no scope.

**Edition.** `edition()` in `lib/api/server.ts` reads `ServerInfo.edition` once per request. The
fixture adapter reads `BEE_FIXTURE_EDITION` (default `cloud`); `BEE_EDITION` overrides both
for local development. An unreachable server or an empty field means `enterprise`.

**Live, no polling.** The activity stream is one `Subscribe`. Server-rendered screens that change
under you (change detail, change list, runs, run detail, issue detail, import progress) mount
`LiveRefresh`: one `Subscribe` for the repo filtered by kind; after the retained events replay,
each event that concerns the screen re-renders it from the server. An event stamped after the
page loaded counts as live even while the replay is still streaming.

**Contract.** `lib/api/contract.test.ts` runs every screen-level read and write through the fixture
adapter and through connect-web against a fake bee.v1 server (`@connectrpc/connect-node`
serving the same descriptors) and requires identical results; it also checks that the G1-G11
fields reach the screens from bee.v1 and that a service a server does not serve fails with
reason `UNIMPLEMENTED` without disturbing anything else.


## 10. Privacy and performance

No analytics, trackers or third-party requests (fonts are bundled; evidence media is
same-origin, §9; tested). Tokens live in an
httpOnly cookie. Runtime dependencies are limited to Next, React, two Radix primitives, cmdk,
sugar-high, `@bufbuild/protobuf`, `@connectrpc/connect` and `@connectrpc/connect-web` (dev:
`@bufbuild/protoc-gen-es`, `@connectrpc/connect-node`).

## 11. Server gaps and honest degraded states

The stand-in service `apps/web/proto/beehive/web/v1/gaps.proto` is retired: gaps G1-G11
(amendments, 2026-10-18) are fields of `proto/bee/v1`, and the mapping reads them directly
(agent model/session/open changes, token budget used, policy replay, audit target and rule,
`denied_only`, `principal_ids`, inline run jobs and CR ids, queue titles, org leases, secret
severity).

Against `bee-server` (docs/spec/server.md §8.3) some screens are degraded by what the server
serves today. Nothing is faked; each shows the server's reason:

| Screen | Server behaviour | What the UI does |
|---|---|---|
| Search | `SearchService` answers `UNIMPLEMENTED` | disabled query box and an "not available on this server" state with the reason |
| Settings · Policy | `PolicyService` answers `UNIMPLEMENTED` | shows `.beehive/policy.toml` read through `QueryService`; Evaluate disabled with the reason |
| Agents | no RPC registers agent definitions (docs/spec/agents.md §9), so `ListAgentPrincipals` is empty and `GetAgentDashboard`/`ListAgentWorkItems` answer `NOT_FOUND` | agent principals that are org members are listed as **unregistered**; their numbers and provenance come from the repo's history (`QueryService.Log` meta), change requests and leases |
| Agent tokens | `TokenInfo` carries only `expires_at` of the scope and no budget limits; revoked tokens are not listed | "Scope not reported by this server"; budget `used` shown without a limit; revoking removes the row |
| Activity (org scope) | `ListLeasesRequest.org` (G10) is refused | leases are read per repo |
| Audit log | `denied_only` (G5) is ignored and denials are not audited yet | the filter is applied to what arrives, so the denied-only view is empty |
| Change detail | the landing principal's own approval does not count; in `--dev` there is one human | a human's approval shows on the change, and the authoring agent (signed in with its token) sees "Policy allows landing" and lands |
| Import | no GitHub App configured in `--dev`; import progress has no events except `import.finished` | the server's reason (`INVALID_STATE`, `UNAUTHENTICATED`) is shown; progress refreshes on `import.*` events and on "Check status" |
| Sign-in | GitHub device flow is not configured | the device button reports it; token paste works |
| Anonymous access | A signed-out caller reads a public repo only when the server allows it; every other action answers `unauthenticated` | public pages render read-only with a Sign in prompt where a write would be; a private or missing repo is the 404 page, which does not say which |
| `allow_anonymous_public_read` | Server config key under `[forge]`, default true; enterprise policy can turn it off | when off, signed-out visitors see the landing and `/signin` only; discovery pages answer with the sign-in prompt; the setting is shown read-only in `/admin` |
| Edition source | `ServerInfo.edition` is `cloud` or `enterprise` (`[server] edition`, default `enterprise`) | the UI reads it per request (§9); an unreachable server or an empty value means `enterprise`, so cloud-only surfaces never appear by accident |
| Discovery | `ListTrendingRepos`, `ListTrendingDevelopers`, `ListLeaderboard`, `GetTopic` or `ListFeaturedTopics` answers `UNIMPLEMENTED` | that block shows the "not available on this server" state with the reason; the rest of the page renders |
| Billing | `BillingService` answers `UNIMPLEMENTED` | `/pricing` and org billing show the "not available on this server" state, never invented plans |

## 12. End-to-end tests

`scripts/check-web-e2e.sh` builds `bee-server`, starts `--dev` on a free port with a temporary
data dir, seeds it through the public API (`apps/web/e2e/seed.ts`, generated bee.v1 clients over
`@connectrpc/connect-node`; blob ids need BLAKE3, `e2e/seed/blake3.ts`), builds the UI and runs
every Playwright spec against `next start` pointed at that server. The seed creates two agents with
budgeted tokens, landed history with provenance, six change requests (revisions, an approval made
stale, a thread, a suggestion, a queued land, a closed one), CI runs executed by a runner speaking
the `RunnerService` job protocol (one failing with an annotation), issues with sub-issues on a
board, leases, releases, a SARIF upload, an npm package, a Pages site, a team, an app and a second
repo. Specs that change state create their own objects; `setup` signs in through the UI once.
Screenshots of the main screens at 1440 and 390 px go to `apps/web/e2e/screenshots` (gitignored).
The seed also creates changes with briefs and evidence: `ui` (before/after, a captioned video, an
ask, and a screenshot made stale by a second revision), `cache` (a benchmark, 120 → 85 ms p95),
`fix` (a CI-verified test report), `rotate` (risks and a rollback) and a docs change that owes
nothing. Lighthouse (performance ≥ 0.90 on the Brief tab of `fix` and `ui`) runs when a
`lighthouse` binary is on PATH.

**Edition matrix.** The suite runs twice, once per edition. The server starts in `--dev` with
`--config` setting `[server] edition` to `cloud`, then `enterprise`; the web build gets the same
value through `ServerInfo` (or `BEE_EDITION` where no server is involved). The shared specs
(home signed in, repositories, repo home, profiles, explore, trending, topics, leaderboards, feeds,
prompts, anonymous read of a public repo, a private repo answering the 404 page when signed out)
must pass in both. Edition specs assert the differences in §13: cloud shows the marketing landing,
`/pricing`, `/new` and org billing; enterprise shows the instance landing, answers 404 on `/pricing`
and `/orgs/[org]/settings/billing`, and shows no self-serve sign-up copy. A spec for
`allow_anonymous_public_read = false` proves signed-out discovery degrades to the sign-in prompt.
The legacy redirects are covered by one spec that follows each 308. Axe runs on every new screen in
both themes and both editions.

## 13. Editions

One binary and one web build serve two editions. The server's `[server] edition` (`cloud` or
`enterprise`, default `enterprise`) reaches the UI as `ServerInfo.edition`. Everything below the
header that is not listed here is identical.

| Surface | Cloud | Enterprise |
|---|---|---|
| Signed-out `/` | marketing landing | instance landing |
| Sign-up | self-serve + `/new` onboarding | SSO/SCIM/admin; creation governed by enterprise policy |
| `/pricing` and org billing | shown | hidden |
| `/admin` | platform operators | site admins |
| Discovery | same | same |

Hidden means the route answers the 404 page and no link, command-palette entry or sitemap line
points to it. Cloud-only code paths are chosen by the edition value, not by build flags, so one
image runs either edition. Both editions use the same names: Beehive for the product, Bee for the version control system.
