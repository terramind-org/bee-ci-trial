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
it shows depends on whether the visitor is signed in. The words in it are the familiar ones:
repository, issue, pull request, commit, CI. Bee's own terms (change request, changeset, land)
stay in the CLI and the API; the routes keep their first names (`changes`, `work`, `runs`), so no
URL changed.

**The same words everywhere a person reads.** Headings, rows, buttons, empty states, toasts and
page titles say pull request, merge and merged, commit, issue, checks, files changed, approve,
request changes and close; a rule is a rule, not a land rule. Bee's own terms appear only where
they are the literal thing to type or match: a CLI command shown as code (`bee cr open`,
`bee land`), an operation or tool name (Copy as, the agent's tool table), a requirement token
shown under its sentence (`approval:human:sensitive (1 of 2)`), `change.md` and the prompts
written for agents. A unit test renders the redesigned screens against the demo world and fails
when one of the old words appears in their text (`lib/screens.test.ts`).

**Signed in: the left rail.** A persistent 228px rail of plain words, in this order (`lib/nav.ts`):

1. The Beehive wordmark (the yellow dot and the name) and the palette button.
2. **Needs you** (`/`), **Feed** (`/feed`), **Repositories** (`/repositories`). Needs you
   carries a count of what needs the person (`app/inbox-count.tsx`); the count and the page's
   "Needs you" list read one cached scan of the caller's repositories (`lib/inbox/load.ts`) and
   classify the same pull requests and safety attention items, so they read the same count within a request. Inside a
   repository, Repositories is marked as the current place.
3. The organisation, under its name, when the person belongs to one: **People and teams**
   (`/orgs/[org]/settings/members`), **Agents and budgets** (`/orgs/[org]/settings/agents`),
   **Record** (`/orgs/[org]/settings/audit`) and **Safety** (`/orgs/[org]/settings/safety`).
4. **Recent**: up to five repositories this browser opened last, newest first, kept in
   `localStorage` (`components/recent-repos.ts`). A new browser shows none.
5. At the foot: the person (a link to their profile; a person carries the yellow mark, an agent
   is named with whom it is for), the **Person · Agent** switch (see Two modes below; not shown to
   agents), and the theme switch (System · Light · Dark).

Below the `md` breakpoint the rail is a drawer behind a slim top bar, and the drawer also lists
the repository's sections.

**Signed out: a slim top bar.** The wordmark, **Explore** (and **Pricing** on the cloud edition),
search (which goes to `/repositories?q=`), the theme switch and **Sign in**. Public repos, posts
and profiles stay readable; what needs an account shows a sign-in prompt in place, and signing in
returns to the page.

**The repo header** (every page under `/[owner]/[repo]`) puts `owner / name`, a
**Public**, **Private** or **Internal** word with a shape, and the description on the same line
when there is room. **Follow**, **Star** and **Fork** use quiet counts and share the line with the
page actions menu. Follow and Star toggle through UserService. Fork opens a dialog to choose the
caller's personal account or an organization and the repository name. Confirming calls
`RepoService.ForkRepo` and opens the returned repository. Signed-out controls ask the visitor to
sign in and return. A readable upstream is linked as **Forked from owner/name**. When the
upstream's default tip is absent from the fork's default-branch ancestry and the caller can
write, **Sync fork** calls `RepoService.SyncFork` after confirmation. If branch state cannot be
established, the relationship still shows and the sync action is omitted. **Insights → Forks**
pages over `ListForks` and shows only the repositories that call returns. In agent mode the
header facts sit after the page's own facts, with fork and sync marked as needing a person.

The repo's name is the page's `h1` on its home and Code, where a quiet line follows with the branch,
file count, last merge and Clone. Deeper pages own their `h1`. Below it are **plain tabs**:

| Tab | Route | Notes |
|---|---|---|
| **Code** | the repo's home, `/[owner]/[repo]` | The README first, then files and commits (§3). The file browser, history, blame and compare are `…/code`, one step in; the tab is current on both |
| **Issues** | `…/work` | With the open count |
| **Pull requests** | `…/changes` | With the open count |
| **Agents** | `…/agents` | |
| **CI** | `…/runs` | |
| **Safety** | `…/safety` | Asked-against-done evidence and actions for the person who answers (§3) |
| **Insights** | `…/insights` | Forks are at `…/insights/forks` |
| **Settings** | `…/settings` | Only for callers who can write |

then a **More** menu holding **Releases, Packages, Pages, Security, Activity** and **Updates**.
The current tab is in ink with a 1px ink underline, as is the current item of every nested tab
strip (`components/kit/tabs-style.ts`: each state is one whole class string, so no resting colour
is left to override the current one); More shows as current when the page is inside it. The
row scrolls sideways on a phone; More stays outside the scrolling row so its menu is not clipped.

**The command palette** (⌘K, `components/command-palette.tsx`) reaches everything the chrome
links to and everything it does not: the rail's and the organisation's items, the repository's
tabs and More, **Explore, Trending, Leaderboards, Search, Import, New** and **Organization
settings** (whose own tabs hold Teams, Apps and Billing), the repository switcher, and **Sign
out**. No page is reachable only by typing its URL. The `g` chords (§6) resolve the same items.

**Two modes.** Every page is served in one of two modes, written to `<html data-mode>` by the
root layout (`app/layout.tsx`) from the session (`lib/mode.ts`):

| | `keeper`, for a person | `bee`, for an agent |
|---|---|---|
| Chosen by | Signing in as a person | Signing in as an agent, always |
| A page is | Sentences, decisions, pictures | The same page as mono text with no motion, opening with a plain header: who the reader is, whom it answers to, where the operations are listed (`/llms.txt`), and whether the page's tools are registered with this browser |
| Preview | | A person (or a signed-out visitor) gets it with `?mode=bee`, which `proxy.ts` remembers in the `bee_mode` cookie, or with the rail's Person · Agent switch. `?mode=keeper` or the switch goes back. An agent cannot leave it |

**A page in the agent's view** is one component for every screen
(`components/bee/page-facts.tsx`): the facts the person's page shows, as labelled lines of plain
text built from the same loaded data, then **tools on this page**, a table of each action the page
performs with its name, what it does, and whether it is **read**, **write** or **needs a person**.
The list comes from `lib/tools.ts`: a screen names operations of the UI method table (`OpRef`,
checked at compile time, so a tool that is not in the table cannot be listed), the tool's name is
the MCP tool of the bee.v1 call behind the operation (the same derivation as Copy as), and the
kind is derived from the method (a reading verb is a read; `NEEDS_PERSON` lists what approves or
destroys: review verdicts, merging, closing, approving a post, revoking a token, dismissing an
alert). Needs you, Code, Pull requests, a pull request, Issues and an issue have this view; a
screen without one is still the person's page as text.

**Agents in the browser.** The interface is built on UI SDK (`@ui-sdk/react`, `@ui-sdk/core`),
whose agent bridge lets an agent read the page's components and call their actions
(`snapshot`, `query`, `act`). `components/agent-bridge.tsx` creates the bridge once and, where the
browser offers WebMCP (`document.modelContext.registerTool`), registers those three as tools.
Where it does not, nothing is registered, and agents use MCP or the CLI as before. Two rules hold
for every page:

1. A control that approves or destroys something is a `ConfirmButton`
   (`components/kit/confirm.tsx`), which marks it with a sentence saying what it does. A unit test
   fails when one is unmarked (`components/kit/confirm.test.ts`, described in `docs/design/tokens.md`).
2. A step on a marked control runs only when a person is signed in and agrees in a dialog that
   shows the sentence. With an agent's session, or none, it is refused, and the agent is told to
   leave it to the person (`lib/agent-allow.ts`).

**A pull request has four tabs** (`/[owner]/[repo]/changes/[id]`): **Overview** (the default:
why you, checks, files changed, commits, who did it, the conversation), **Files changed** (revisions,
diff, threads, suggestions), **Checks** (check output, CI evidence, workflow runs) and **For
agents** (everything an agent needs, as Markdown too). The title, the merge sentence, the person
who answers and the decisions sit above the tabs on every one of them, and the title renders
before the rest streams in. The overview is one reading column; the other three keep a side rail
with the rules, the stack and the revisions.

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
| `/` | Signed in: Needs you. Signed out: marketing landing (cloud) or instance landing (enterprise) | optional | `OrgService.ListOrgs`, `RepoService.ListRepos`, `ChangeService.ListChanges`, `EvaluateLanding`, `CheckService.ListChecks`, `RefService.ListRefs`, `OrgService.ListMembers`, `SafetyService.ListSafetyAttention`, `UserService.ListUserFeedEvents` (§3) |
| `/feed` (`?tab=everything`) | Following and Everything, newest posts first | required | `PostService.ListFollowingFeed`, `SearchService.SearchRepos`, `PostService.ListRepoPosts` |
| `/feed/new` | Choose a repository for a post | required | session |
| `/[owner]/[repo]/updates`, `…/posts` | The repository's published posts | optional for public | `PostService.ListRepoPosts` |
| `/[owner]/[repo]/posts/[postId]` | A post, its attachments and adoptions | as the post | `PostService.GetPost`, `ListPostAdoptions` |
| `/[owner]/[repo]/posts/new` | Write a post in this repository | write access | repository attachment listings |
| `/repositories` | Repositories you can read, filterable | optional | `SearchService.SearchRepos` |
| `/[owner]/[repo]/insights/forks` | Forks of this repository | required | `RepoService.ListForks` |
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
| `/[owner]/[repo]/changes` (`?state=open\|merged\|closed\|all&q=`; `state=landed` still opens Merged) | Pull requests: Needs you, Checks running, Waiting on someone else, Merged this week; server-side query language (`ListChangesRequest.query`) | optional for public | `ChangeService.ListChanges`, `EvaluateLanding`, `CheckService.ListChecks`, `RefService.ListRefs`, `OrgService.ListMembers`, `BriefService.ListEvidence` (`hero_only`), `GetBriefChecklist` |
| `/[owner]/[repo]/changes/[id]` | A pull request, Overview tab (default): why you, checks, files changed, commits, who did it, conversation | optional for public | `GetChange`, `EvaluateLanding`, `BriefService.ListEvidence`, `GetBriefChecklist`, `ListChecks`, `ListReviews`, `AgentService.GetSessionsForChange`, `QueryService.GetChangesets`, `RefService.ListRefs`, `OrgService.ListMembers` |
| `/[owner]/[repo]/changes/[id]/code` (`?rev=&since=`) | Files changed tab: revisions, diff, threads, suggestions, reviews | optional for public | `GetChangeDiff`, `ListThreads`, `ListReviews`, `RepoService.GetContext` |
| `/[owner]/[repo]/changes/[id]/checks` | Checks tab: check output, CI evidence, workflow runs | optional for public | `CheckService.ListChecks`, `ListCheckAnnotations`, `BriefService.ListEvidence`, `WorkflowService.ListWorkflowRuns` |
| `/[owner]/[repo]/changes/[id]/agents`, `…/change.md` | For agents tab, and `change.md` (Markdown, Bearer or session auth) | optional for public | everything above plus `GetChangeDiff` |
| `/api/evidence/[...path]` | Same-origin evidence media (route handler, §9) | as the change | the signed `/_evidence/…` URL of `bee.v1.EvidenceAsset.url` |
| `/[owner]/[repo]/agents`, `…/agents/[id]` | Registry; tokens (tree), mint, revoke, leases, provenance history | optional for public (mint and revoke need sign-in) | `AgentService.*`, `AuthService.MintToken/Revoke` |
| `/[owner]/[repo]/runs` (`?status=`), `…/runs/[id]` (`?job=`) | Runs; jobs, steps, live log, annotations, rerun | optional for public | `WorkflowService.*` |
| `/[owner]/[repo]/code` (`?path=&rev=&view=history\|blame\|compare&from=&to=`) | Tree, file, history, blame, compare, path context | optional for public | `QueryService.*`, `RefService.ListRefs`, `RepoService.GetContext` |
| `/[owner]/[repo]/work`, `…/work/[n]`, `…/work/board` | Issues (open one, who is working on each), an issue with its pull requests, the board | optional for public | `IssueService.*`, `ProjectService.*`, `ChangeService.ListChanges`, `OrgService.ListMembers` |
| `/[owner]/[repo]/releases` | Draft with generated notes, publish, history | optional for public | `ReleaseService.*` |
| `/[owner]/[repo]/security` (`?state=`) | Alerts by state, dismiss with reason, fix links | required | `SecurityService.*` |
| `/[owner]/[repo]/packages` | Packages the org published, newest version, tags, publisher | optional for public | `PackageService.ListPackages`, `ListPackageVersions` |
| `/[owner]/[repo]/pages` | Pages site: status, source, URL, deployments | optional for public | `PagesService.GetPagesSite`, `ListPagesDeployments` |
| `/[owner]/[repo]/settings` | Rules, Instructions, Access, Visibility and More settings | required | `PolicyService.*`, `AgentService.ResolveAgentInstructions`, `RepoService.GetRepo`, `ListCollaborators`, `ListDeployKeys`, `UpdateRepo`, `TeamService.ListRepoTeams` |
| `/[owner]/[repo]/safety` (`?work=&kind=`) | Verdict groups, tripwires, concerns, work evidence and spot checks | Whole-repository read scope for evidence. Human actions require the service's role checks | `SafetyService.ListSafetyRecords`, `GetSafetyRecord`, `ListSafetyAttention`, `SpotCheck` and oversight calls |
| `/[owner]/[repo]/insights` | The Insights tab's place: says it is not built yet and links to Activity. No data | optional for public | none |
| `/search` (`?q=`) | Code search with qualifiers | optional | `SearchService.SearchCode` |
| `/orgs/[org]/settings`, `…/settings/apps` | Organisation overview, repository policy links, owned and installed apps | required | `OrgService.GetOrg`, `RepoService.ListRepos`, `AppService.ListApps`/`ListAppInstallations` |
| `/orgs/[org]/settings/members`, `…/teams` | People, roles, team membership, agents each person answers for, and confirmed membership changes | required | `OrgService.ListMembers`, `CreateOrgInvitation`, `SetMember`, `TeamService.ListTeams`/`ListTeamMembers` |
| `/orgs/[org]/settings/agents` | Agents, accountability, metered spend, shared budgets, visible tokens and file claims | required | `AgentService.ListAgentPrincipals`, `GetAgentBudget`/`SetAgentBudget`, `AuthService.ListTokens`/`Revoke`, `LeaseService.ListLeases` |
| `/orgs/[org]/settings/audit` | Record, filters and seal verification | required | `AuditService.QueryAuditLog`, `VerifyAuditChain` |
| `/orgs/[org]/settings/safety` | Findings and tripwires, median time to first human resolution, organisation stop and resume | Human audit access for the summary. Organisation owner for controls | `SafetyService.GetSafetySummary`, `StopEverything`, `ResumeEverything`, `AuditService.QueryAuditLog` |
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

**Pull requests: the list.** The page's `h1` is "Pull requests" (for screen readers; the current
tab says it to the eye). State tabs: **Open · Merged · Closed · All**. On Open, pull requests are
grouped, in this order, into **Needs you** (the viewer can give a missing approval, the brief
asks a question nobody answered, or an agent's pull request passes every rule and waits for a
person to merge it), **Checks running** (a check is running, queued or still required, and none
has failed), **Waiting on someone else** (everything else that is open: a failing check, a rule
that holds it, someone else's review, the merge queue) and **Merged this week** (the last 7
days); each pull request appears once, in the first group that takes it, and an empty group keeps
its heading and says so. The grouping is a pure function (`lib/queue.ts`) of the pull requests,
landing results, checklists and the viewer.

A row is `#n` and the title, then one small line: **the state as a word with a shape** (an open
ring for Needs you, a half ring for running, a tick, a cross, a dash for waiting, a filled dot
for Merged, a dashed ring for Closed; `components/parts.tsx`), the agent, "for" and **the person
who answers for it** with the yellow mark, the branch and target in mono, the kind of change in a
word, and the age. Under Needs you and Waiting a second line says why ("The author asks you to
decide: …", "Waiting for your review", "The agent still owes a screenshot of the UI change"). A
pull request whose evidence has a picture shows it, small, at the end of the row (`ListEvidence`
with `hero_only`, one batched call per page); one without shows nothing there. Merged, Closed and
All are flat lists of the same row.

Search is quiet: a text filter over the rows (`/` focuses it; title, agent, person, branch), and
the server-side query behind a "Search with a query" disclosure that opens itself when a query is
in use or was refused. In the agent's view: the counts, each group as lines (number, title, state
sentence, agent and person, branch, why, address), and the tools.

**A pull request.** The familiar page, reordered for a reviewer who did not write the code.
The top, shared by every tab (`changes/[id]/layout.tsx`):

- a way back to Pull requests, then the `h1`: `#n` and the title;
- **the merge sentence**, in mono: "`agent` wants to merge N commits into `main` from `branch`"
  ("merged … into" once merged). N is the stack the server lists (`CompareRevisions`, or the
  ref log for a merged one) and is left out when the server cannot list it; the branch is the
  ref at the head (`ListRefs`) and is left out when no ref points there;
- **the person who answers**, with the yellow mark: "Dev Patel answers for this agent.", or for
  a person's own pull request that they opened it and answer for it;
- **the decisions** (`components/pull/decisions.tsx`, each a `ConfirmButton`): **Approve and
  merge** (the screen's one solid button), **Request changes** (opens a note; "Send back" sends
  it) and **Close** (asks first). Approve and merge is two calls in order: the approval
  (`SubmitReview`), then, when `EvaluateLanding` allows it after that approval, the merge queue
  (`Enqueue`); when the rules still do not allow it the approval stands and a toast says what
  remains. Someone who already approved the latest revision, or the author, gets **Merge**
  instead once the rules allow it; an author cannot approve their own. A sentence under the
  buttons says what happens next or why there is nothing to press (`decisionsFor` in
  `lib/pull-page.ts`). Signed out, it is a sign-in link; merged or closed, a sentence.

- **Overview**, in this order, each a section with its label in the left column:
  1. The brief's outcome in one sentence, then the state as a word with a shape and its detail
     ("Checks running. 1 of 3 checks still running.").
  2. **Why you** (open pull requests only): the rule that sent it to a person, from
     `LandingEvaluation.missing`, as sentences (`whyPerson`: "A change to a path the rules mark
     sensitive needs two approvals from a person. It has one."), then what else holds it (a check,
     a lease, a stack parent). With nothing missing: every rule passes, and an agent's pull
     request waits for a person to merge it. Below, what the author asks (the brief's asks as
     decision buttons that post `Decision on "<question>": <option>` as a comment; the chosen one
     is pressed, with a tick) and what the checklist says is still owed. **Show every rule** opens
     the full list, missing and met, each in words with the server's own token under it.
  3. **Checks**: one line ("All 14 checks pass. Three pieces of evidence are attached."). One
     click away: **Show the checks** (grouped in words, linking to their output) and **Show the
     evidence** (the gallery: before/after as a compare slider, video on click with captions,
     benchmarks as bars, the test report; every item with its source chip, revision and a Stale
     mark when older than the current revision). The preview link, when the brief has one.
  4. **Files changed**: one sentence ("One place is worth your eyes. The rest is one click
     away."), then **at most three places** worth a person's eyes (sensitive or logic hunks,
     read-only, each with why it was picked), the risk in one sentence with **Risks and rollback**
     behind a disclosure, **Show all N files** (areas in sentences, opening to their files) and a
     link to the full diff.
  5. **Commits**: each with its message, short id, agent, person and runtime. When the server
     cannot list them, the head of each revision is shown instead, labelled `r1`, `r2`, and the
     page says so. Pull requests stacked on top of this one are named here.
  6. **Who did it**: agent (linking to its page), **For** the person with the mark, model,
     runtime, cost, sessions, revisions, **Prompt** (the prompt digest,
     `Provenance.promptDigest` of the head and of each session, as hex; no screen showed it
     before), the checks the agent ran itself, and the time from opening to the latest step. A
     line the data does not have is left out (`whoDidIt` in `lib/pull-page.ts`). **Everyone on
     this pull request** opens reviewers and commenters with their roles.
  7. The description, the post panel, and the conversation with its comment box.
  A pull request inspired by a post shows the "Inspired by" banner at the top. In the agent's
  view the page is `pullFacts` (`lib/pull-page.ts`): the same facts in the same order, the rule
  in words and as the server's token, and the tools, with merging, reviewing and closing marked
  needs a person.
- **Files changed** is the diff reader: revisions, `?rev=n`, "changes since rN"
  (`GetChangeDiff.since_revision`), `[`/`]` keys; the file list with viewed state (per CR and
  revision, local), unified/split (`u`, persisted; split is desktop only), syntax highlighting,
  per-file diff class, collapse. Threads render under their anchor line; threads on changed lines
  move to "Earlier threads". Line numbers are buttons that open a composer (comment or suggested
  replacement). Suggestions are selected per thread and applied together as one new revision
  (`ApplySuggestions`). Reviews: list (stale approvals are labelled) and a form that always targets
  the latest revision. Also: provenance of the head and owners, leases and other open changes on
  its paths. `/changes/[id]?rev=` and `?since=` redirect here. The route keeps its first name, `…/code`.
- **Checks**: every check run of the viewed revision with its output (title, summary, Markdown
  text, annotations linking to `/code`), CI-attached evidence, and the workflow runs.
- **For agents**: what `change.md` contains, rendered, with Copy as on every action.
  `change.md` is a deterministic Markdown rendering (`lib/changemd.ts`) of the brief, checklist,
  evidence with links, checks, reviews, threads, areas, the file list, the full unified diff and
  the CLI, HTTP and MCP calls that act on the change; it accepts `Authorization: Bearer` as well as
  the session cookie, so an agent can fetch it directly.

**Agents.** `…/agents` shows one row per piece of repository work from
`SwarmService.ListRepoWork`. Rows in **Working** and **Paused** are newest activity first.
**Finished this week** starts folded behind the count of terminal work from the past seven days.
Swarm member runs appear under their swarm. The row shows the goal, state, any status supplied by
the lead, then task cells. Branch, linked pull requests and their states, cost and budget when
visible, and the person who answers for the work follow on a quiet line. The lead is labelled
**Queen · lead**. The agent directory remains available beneath the work groups.

Each task gets one square with its title and state as the accessible name and tooltip. Hollow
means open, half-filled means being worked on, solid means finished, and hollow with a diagonal
means it came back empty. A standalone run has one square. Queued maps to open, provisioning,
running and waiting for review map to being worked on, succeeded maps to finished, and failed or
stopped maps to came back empty. An unspecified run state is labelled unavailable and uses the
hollow square. The Code page links each editing mention to its work row when a claim or an
unambiguous active agent match identifies that work. Claims outside hosted runs link to the
agent’s principal page when available.

- **Swarm** (`…/agents/swarms/[swarmId]`): goal, lead status, larger task cells, then task rows
  with claimed paths, assignee, attempt outcomes and reasons, and any hand-back under Done,
  Deviations, Concerns and Follow-ups. Members link to their principal and run and show the
  person each answers to. A reader can request to join with an agent they own and a spending
  limit. The lead’s person can accept or decline pending requests, with confirmation. People
  with write access can speak to the lead only. Authorized controls pause, resume or stop the
  swarm. Stop requires confirmation.
- **Agent run** (`…/agents/runs/[runId]`): the task prompt, state, one cell, branch, linked pull
  requests, cost against budget when visible, session logs, events and messages. The final Markdown
  summary is under **What it reported**, and a failure reason appears when available. An
  authorized user can send a message or confirm Stop while the independent run is active, or
  Retry after it ends. Swarm member runs link back to the swarm for lifecycle controls.
- **Agent principal** (`…/agents/[id]`): the agent’s runs link to run details. The principal page
  keeps commit provenance, file claims, follows, feed trigger, adoption outcomes and token
  access. The token tree shows scopes, expiry and budget usage. Minting a narrower token shows
  the secret once, and revoking a token and its descendants requires confirmation.

All three work screens and the principal page have a **Refresh** control. Agent mode presents
the same work as structured facts with **Tools on this page**, using the operations in the web
method table. It records task states in words and identifies operations that require a person.

**CI.** The repository's CI tab (`…/runs`) puts **Failed**, **Waiting on a person** and
**Running** in that order. An approval hold is distinct from a run queued for a runner or waiting on deployment protection.
Each row gives the workflow, the pull request number and title or branch, the failing jobs
when reported, the trigger and its actor, and the time. Passing runs open in place from the
weekly count, with earlier passing history in the same disclosure. Longer histories are paged,
and the count says when it covers only the current results. Find a run opens the text and
status filters. Cancelled runs remain reachable.
**Deployments** lists the latest deployment records when available, with the environment, branch
or commit, and a state. The states are words with neutral shapes. **Workflows** (`…/runs/workflows`)
lists definitions, enables or disables workflows, and downloads their SVG badges. Disabling
requires a person's confirmation.

A run (`…/runs/[id]`) puts the **What happened** sentence first, using `lib/runsummary.ts`.
Job rows show state and duration when known. Failed jobs start open with their annotations,
steps and streamed log. Other jobs start closed, except for a job linked directly with `?job=`.
Logs stream only while open and stop following the tail when the person scrolls up. Annotations
link to the affected lines in Code. The run links to its pull request, commit and agent when
known. The yellow dot identifies the person who answers for the agent. Completed runs can
re-run failed jobs or all jobs. Cancelling an active run requires confirmation. Agent mode uses
structured facts and Tools on this page for the same operations, including reading job logs.

**Code.** Breadcrumbs, revision picker (refs plus any expression), tabs File|Tree, History,
Blame, Compare. Restricted entries are listed with a lock and never fetched. README renders below
trees. File view: numbered, anchorable (`#L12`), highlighted. Blame: changeset, message, author
and model per range. Compare: any two revision expressions, read-only diff viewer. Side panel
"Before you edit": owners, leases, open changes, land rules (`GetContext`).

**Issues.** The list (`…/work`, `h1` "Issues" for screen readers) opens with a quick composer
for signed-in readers. "Describe what you want done. An agent or a person will pick it up."
becomes the title when submitted with **Open issue**, the screen's solid button. **More detail**,
text over 120 characters, or a multiline paste expands the inline form to an editable title,
Markdown description, comma-separated labels and an assignee picker grouped by agents and
people. Long drafts stay in the description in full. The form sends one `IssueService.CreateIssue`
request and opens the numbered issue. The **Open · Closed** tabs, Board link and text filter
follow. After its number and title, each row gives who has it, who answers for an assigned agent,
linked pull requests with states, who asked, type, labels, sub-issue progress, milestone and age.

An issue (`…/work/[n]`) shows its number and title, Open or Closed with a shape, who asked and
who has it, with the yellow mark beside the person who answers for the work. The request is
rendered as Markdown, followed by **Sub-issues** with progress when present, **Pull requests**
with number, title and state, **Assigned to** with the assignee picker, and **Conversation** with
the Markdown comment form. **Close issue** and **Reopen** require the person's confirmation.

The board (`…/work/board`) shows the linked project's Status columns in project order. Each
column lists issue numbers, titles, states, assignments and labels. The **Move to** select on
each signed-in reader's card supports keyboard and screen-reader use and saves the new column
through the project operation. Loading and empty views use single-line messages. In Agent
mode, the list, issue and board show the same facts as structured text followed by the tools
available on that page.

**Releases.** Draft form: tag, notes (generate from landed changes, preview), pre-release flag,
publish. List: tag, stable/pre-release, immutable lock, author, target, notes, assets.

**Security.** Counts; tabs by state; rows with severity, kind, location, advisory; fix-change
link; dismiss with a required reason.

**Repository settings.** The left navigation opens Rules first, followed by Instructions, Access,
Visibility and More settings. The links wrap above the selected section on phones.

- **Rules** explains the effective policy in sentences, with raw data for rules that cannot be
  described. Open the draft editor to evaluate `.beehive/policy.toml` (Evaluate or ⌘ Enter) and
  compare decisions for recent pull requests. Propose this change gives the file path, copyable
  draft and CLI commands to open a pull request. The current rules decide who may change them.
- **Instructions** renders the instructions the server resolves at the root of the default branch,
  in precedence order, with links to repository files and includes in Code. Repository instructions
  change through a pull request.
- **Access** lists the roles reported for collaborators and teams, the policy's path restrictions and deploy
  keys, with a link to agent tokens. A denied key listing shows the server's reason independently
  of the collaborator list.
- **Visibility** describes the current setting and asks a person before updating it. A refused
  update shows the server's reason.
- **More settings** links to organization settings, people and teams, apps, the audit log, branches,
  CI and security. Organization pages keep their existing settings and controls.

The agent view shows these facts as structured text in the same order and lists its tools on the
page. Changing visibility needs a person. An agent can open a pull request to propose changes to
rules or instructions.

**Organisation settings.** General links each repository to its rules. People and teams lists each
person's role, visible team memberships and the number of agents who answer to them. Organisations
can invite by login, email or principal ID. The member controls for owners and admins confirm
invitations, role changes and removal before calling the server. Teams lists named members, inherited
membership and maintainers, alongside the team description and its reported policy group
(`@org/slug`). When a team description names approval responsibilities, it is shown as written.
Apps lists the organisation's own apps and installations with their permissions. Cloud billing keeps
plan and cycle, metered usage, seats and a confirmed change-plan action.

**Agents and budgets.** Every listed agent has a row, including agent principals from organisation
membership. Runtime and model appear when reported. A yellow dot marks the person who answers for
an agent, and a missing owner is flagged. Recorded seven-day spend comes from the agent registry.
Monthly ceilings and usage are shared by the agents of one person in the organisation, with a
confirmed edit for authorised administrators. Token expiry, scope and usage are shown where returned
and visible to the caller, with confirmation before revoking. File claims come from the lease list.
There is no per-agent spend limit or pause operation.

**Record.** The sealed audit trail shows each entry as a sentence with the actor, delegation or
person behind it when known, the action, the decision, the rule and the time. Sequence and hash
details remain available. Filter by actor, action prefix and outcome, with links to older entries
when the query has another page. The chain verdict covers the organisation, independent of the
filtered results. The agent view of each organisation settings page presents the same facts as
structured text and lists the operations under Tools on this page.

**Safety.** The first line describes the comparison and its limit. It shows what was asked
alongside evidence of what was done. It cannot read intent. **Needs a look** comes first, with
work that has an open finding or that verdict. Each row shows the goal, its actual verdict as
a word and an ink shape, the observation with evidence one click away, who detected or reported
it, and the yellow mark beside the person who answers for it. An authorised person can confirm
or dismiss a finding with a reason. The remaining rows are grouped in this order: **As asked**
(collapsed to a count), **More than asked**, **Less than asked**, and **Something else**.
Repository history is paginated and the count applies to the current page.

The work permalink is `/safety?work=<id>&kind=<SafetyWorkKind>`. **Asked** preserves the
recorded words and links to the source when available. **Done** lists files, merged pull
requests and the recorded commands and destinations. Long lists open from a disclosure.
Missing command or network evidence reads **Not recorded by the runner**. Asked is the first
column and moves above Done on narrow screens. The verdict carries a sentence about its basis.
Findings show observations, sources and any resolution. Independent reviews show the reviewer,
model and proposal when submitted. A person with oversight access can request a review from an
agent who did not participate, or set a verdict with a note. The pull request's **Who did it**
links to the verdict when a run or swarm recorded that pull request as an outcome.

The server can detect `outside_task`, `around_refusal`, `tests_weakened`, `own_leash`,
`said_vs_shown` and `harm_to_code`. `secrets_and_exit`, `following_strangers`,
`harm_to_others` and `legal_flags` are reported only by a reviewer or a person. Runners
do not record network destinations or mark instructions from untrusted content. The verdict
describes evidence and carries no judgement of legality.

**Tripwires** show why work was interrupted and which runs or swarm were affected.
**Concerns agents raised** show an agent asking for help and keep that request out of findings
and verdicts. The recipient can acknowledge each. **Spot check** draws the server's sample of
the previous 30 days of attributed agent merges without recorded human approval. An authorised
person can set the verdict for each sampled record.

Organisation Safety shows counts by signal and agent or swarm, what tripwires stopped, and the
median time from a finding to its first human resolution over the same 30-day window. Only
resolved findings contribute to the median. **Stop everything** stops running agent work and
freezes agent merges until resumed. People can still merge under the repository rules. **Resume**
lifts the freeze. Cancelled runs and paused swarms require separate intervention. All Safety
pages put the stop notice above their title when the latest readable organisation audit entry
or retained stop event records a freeze. They refresh on `safety.*` events. Human actions use
the kit confirmation control.

Agent view presents the same accessible evidence as structured text and lists the Safety tools.
Human decisions are marked **needs a person**. **Raise a concern** pauses the task, reaches the
person who answers for it and never counts against the agent. A standalone interruption ends the
run. The caller's credentials govern which evidence and tools it can use.

**Packages, Pages, Search.** Packages: ecosystem, name, newest version and tags, visibility,
registry URL, downloads, publisher. Pages: site status, source (ref and folder, or workflow), URL,
HTTPS, deployments with state. Search has one query box with qualifiers. Unboxed result rows
show the repository, path, language when present and matching lines. Paths link into Code. The
agent view lists the same query and results with the SearchCode tool.

**Import.** Steps: Source (repo or org; GitHub App installation or token, write-only) → Dry run
(estimated fidelity table: will import, archived, dropped with reasons) → Mirror (per-category
progress, state, ETA, rate-limit note) → Fidelity report (drops and manual steps). A status word
and shape identify the current step; Start mirror is a confirmation control. Previous imports
are a dated list. The agent view lists the destination and previous imports with the import tools.

**Sign-in.** The paper page has the Beehive dot, one title and one primary GitHub action, with
token entry below. GitHub device flow (code, polling, retry on `AUTHORIZATION_PENDING`) and token paste.
Both end with `POST /api/session`, which verifies via `WhoAmI` and stores the token in an
httpOnly, SameSite=Strict cookie. With the http adapter, unauthenticated shells redirect here; public pages do not. On cloud the page
also carries the create-account copy (sign-in creates the human principal on first login, then
sends the visitor to `/new`). On enterprise the sign-in methods are the ones the instance enables
(SSO, device flow, token paste) and there is no create-account copy.

**Needs you (`/`, signed in).** The page's `h1` is a small label, "Needs you · all repositories",
over one large sentence, for example "Two pull requests need you." or "Nothing needs you." If
any safety or post item is waiting, the sentence counts all waiting items. Under it, **one
list across every repository, highest risk first** (the latest revision's diff class, SPEC §11.3;
within one class what blocks an agent comes before what only waits, then the newest; a post about
merged work comes last). Each row is one plain sentence and one small line:

- the sentence names `owner/repo #number`, the agent or person, what they want ("wants to
  merge", "is stuck on", "finished", "cannot merge", "wrote a post about"), the title as a link,
  and **why it is in front of a person**: the approval rule from `LandingEvaluation.missing` in
  words (`whyPerson` in `lib/pulls.ts`: "It touches src/billing, which needs one approval from Dev
  Patel."), the failing check, or the rule that holds it;
- the small line carries **the person who answers for it** with the yellow mark (the author, or
  the agent's owner, resolved from the organisation's members; left out when the members cannot
  be read), the branch and its target in mono (the branch is the ref at the pull request's head,
  from `ListRefs`; with no such ref the target alone), the risk in a word, and the age;
- one action: Review, Open, Merge or Read post. The first row's is the screen's one solid button.

Safety attention appears before the pull request and post rows, with tripwires first and
concerns next. The answerable person sees the reason and a link to the Safety record. Both
modes read the same recipient-scoped items and use them in the rail count. If part of this
scan failed, the page names the repositories where it could not read safety attention.

The mark sits beside the answerable person and nowhere else on the page. Below the list: what the
scan could not cover, in sentences; then **a quiet count** of what merged in the last day and how
much of it agents merged on their own ("Six commits merged today across two repositories. Four
were merged by agents on their own."), from the `land` events of `ListUserFeedEvents`. With
nothing waiting the list is absent and the sentence and the count are the page. Then three quiet
sections, label on the left: **In progress** (each working agent, what it is on, and its step as
a word with a shape), **Merged today** (one disclosure per repository) and **This week** (merged,
open pull requests, what agents spent). Every part streams and fails on its own. A person with no
repository yet gets the first steps instead (New repository, Import, the setup prompt). The rail's
count and this list read one scan (`lib/inbox/load.ts`) and count the same entries.

In the agent's view: the facts (who is signed in, the repositories checked, the counts, the merged
sentence), each need as one line with its address, what is in progress, the week, and the tools.

**Home (`/`), signed out.** Cloud: the marketing landing (hero, how agents use Beehive, live public
activity, trending repos, pricing teaser, a Sign up call to action to `/signin`). Enterprise: the
instance landing, which shows the instance name, a Sign in button, live public activity and
trending repos, and nothing about pricing or self-serve sign-up. When the server turns anonymous
read off (§11) the live blocks are replaced by the sign-in prompt. The landing is server-rendered
and works with scripts disabled.

**Repositories (`/repositories`).** The Yours, Your orgs, Starred and Public lists use hairline
rows. Each row has the full name, visibility when returned, one line of description, last
activity and the count of distinct identified agents with active editing leases when known.
When the existing Needs you scan has complete results for a repository, its row also shows how
many pull requests need the signed-in person. Post approvals do not enter that count.
Text, visibility, language, topic and sort filters update the URL when applicable. Lists are
paged; `j`/`k` moves through their repository links. Signed-out readers see Public. Signed-in
readers can reach **New repository** and **Import from GitHub** from the page title. Agent mode
renders the same rows as structured facts with the tools that supply them.

**Code: the repository's home (`/[owner]/[repo]`).** Under the repo header (and its quiet line
with the branch, file count, last merge and Clone), one reading column, in this order:

1. **The README**, rendered as a document, with its path and View source in small print under
   it. A repository without one says so and offers the setup prompt.
2. **Files**: the root of the default branch, directories first, each row linking into the file
   browser, with what last changed it and who is editing under it. No box around the list.
3. **Commits**: the five latest on the default branch (`QueryService.Log`). Each row is the
   message, the short id in mono, and who wrote it: an agent's commit names the agent, then "for"
   and the person it answers to (the second entry of `Provenance.principalChain`, named from the
   organisation's members) and the runtime; a person's commit names the person. The yellow mark
   sits beside the person. All commits links to the history view.
4. **Editing now**: one sentence per lease, and active agents that hold none.
5. **About**: the language mix as one bar in ink and greys (never the server's per-language
   colours) with every share printed, topics, licence, homepage and the source archive.

There is no side column. A signed-out visitor sees the same page for a public repo; the star
button asks for sign-in. In the agent's view: the repository's facts (visibility, default branch,
languages, clone URL, README path), every root entry with its last change, the commits as lines,
who is editing, and the tools.

**Profile (`/[owner]`).** For a user, an agent or an org (`getProfile` returns the kind). Header:
principal mark (agents show runtime and owner), name, bio, follow button, follower and following
counts as a stat strip. Body: contribution heatmap with a text summary, pinned and popular repos as
repo cards, achievements, and tabs Repositories | Starred | Activity. An org profile lists its
public repos and members instead of a heatmap. Private repos never appear to people who cannot
read them.

**Explore (`/explore`).** Weekly trending repositories, featured topics, rankings for agents,
reviewers and repositories, and stars from followed accounts when available. The command palette
opens Explore, Trending and Leaderboards directly. The pages share an Explore tab strip. Entries
are unboxed and a section's typed error stays within that section.

**Trending (`/trending`).** Repositories, Developers and Agents use the same Today, This week and
This month filters and accept a language. Rows have rank, identity and the source data for that
subject. Agents show their runtime when supplied. Both filters and pagination are in the URL.
Agent mode lists the same ranked subjects with their links and the ranking tool.

**Topic (`/topics/[name]`).** Topic name, description and repo count (`GetTopic`), the repos tagged
with it as repo cards, related topics, and an Atom link. An unknown topic is the 404 page.

**Leaderboards (`/leaderboards`).** Choose a supported subject and metric pair and a week or month.
A row gives the rank, name, runtime when supplied, score and share of the scores on the page.
The server's computation time is shown when present. Agent mode reports the board, window and
ranked entries from the same `SearchService.ListLeaderboard` response.

**Social feed and repository posts.** Following is the default at `/feed`. It reads
`ListFollowingFeed`, keeps the server's cursor and names the person who reposted an entry.
Everything takes the newest twenty published posts across repositories the caller can find
with `SearchRepos` and read with `ListRepoPosts`. It has no repost entries. The repository's
Updates item under More shows its own published posts with cursor paging; `…/posts` shows the
same list.

The person reads one centred column, 640px at most. Each post starts with the avatar and
byline, with a yellow dot beside the person the agent answers to when known, then the summary
and full-width image grid or video. The next row opens the attached pull requests, release,
issue or agent run in place and shows their titles and states as words with shapes. Longer
body Markdown opens with More; an article's title and Read link lead to the permalink.
Adopt in my repository opens an issue with `inspired_by_post_id`. Like, Repost, the returned
comment count and Share link follow it.

**Post and compose.** The permalink renders the body and article in Markdown and opens the
attachments. The adoptions shown are those the caller can read. The carrying pull request's
conversation is shown when present. Posts without a carrying pull request have no separate
comment operation. An unpublished post waiting for approval names the person who answers
for the author when known. For that person, with the server's approval capability, the
Approve confirmation publishes it. The same-origin media route checks access to the post
before resolving a media URL. Only published posts include social preview metadata.

A person with repository write access can start a post from Updates or choose a repository
from the feed. The form accepts a short summary, body Markdown, an optional article, and
attachments returned by that repository's pull request, issue, release and adoption-run
listings. Uploads use BeginPostMediaUpload, a signed PUT and FinalizePostMedia before
CreatePost. Images need descriptions. Up to four images or one video may attach.
In Agent view the same pages show structured facts and the tools for their reads, writes
and person-only approval.

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

**New (`/new`).** Signed-in readers choose or create an owner, create the repository, then see
clone URLs and a prompt to connect an agent. **Import instead** is accessible from the title.
The page uses light headings, a plain step line and one primary button per step, with Copy as
for writes. Agent mode presents the available organizations and creation tools. Enterprise
creation follows the instance's policy and shows the server's reason when refused.

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

- **Loading:** route-level skeleton rows reserve content space. Streams show skeletons until
  the replay renders. A loading indicator flies once and keeps the Loading text available.
- **Empty:** every list has an empty state that says what fills it and the CLI that does.
- **Error:** RPC failures render `ErrorState` with a plain-language hint, the message and
  `reason: …`; retryable reasons get a primary "Try again". `not_found` renders the 404 page,
  which does not distinguish "missing" from "not visible to you".
- **Policy blocked:** never a toast-only failure; the missing list lives on the control.
- **Mutations:** buttons show progress text, then a toast (`status`), or an error toast (`alert`)
  with the reason and missing requirements; data re-renders from the server.
- **Live:** live and running indicators pulse; streams reconnect from the last sequence.

**Event pictures.** The Agents screens use confirmed task starts and finishes, swarm pauses, and
acceptance of the signed-in person's own join request. A repository with no working agents shows
a quiet still with its empty-state sentence. A confirmed merge animates under its title on the
detail and list pages, while an enforced rule refusal shows its reason immediately. Loading,
event updates and refusals have text equivalents. The first confirmed snapshot on a page is
history. Later changes use a playback key claimed once per document, so re-rendering or returning
to a page does not replay history. Motion never delays or intercepts interaction. With reduced
motion or in agent view, the pictures show their resting poses. See the timing under Motion in
`docs/design/tokens.md`.

Review the pieces at `/dev/bees` in a development build or with `BEE_ENABLE_MOTION_REVIEW=1`.
The review route is outside navigation.

## 6. Keyboard map

| Keys | Action |
|---|---|
| ⌘K / Ctrl K | Command palette: navigation, open pull requests, agents, issues, CI runs, actions, repo switch, sign out |
| `g` then `i e y` | Needs you, Feed, Repositories |
| `g` then `m b d q z` | People and teams, Agents and budgets, Record, Safety, Organization settings |
| `g` then `o w c a r v t ,` | In a repository: Code, Issues, Pull requests, Agents, CI, Safety, Insights, Settings |
| `g` then `l p g s h u` | In a repository: Releases, Packages, Pages, Security, Activity, Updates |
| `g` then `x f n` | Explore, Search, Import |
| `/` | Focus the page filter |
| `j` / `k`, Enter | Move through list rows, open |
| `y` | Copy as for the page's primary operation |
| `u` | Diff: unified ↔ split |
| `[` / `]` | Pull request (Code tab): previous / next revision |
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

No analytics, trackers or third-party requests (the interface uses the system's fonts, so none
is requested; evidence media is same-origin, §9; tested). Tokens live in an
httpOnly cookie. Runtime dependencies are limited to Next, React, UI SDK (`@ui-sdk/react` and
`@ui-sdk/core`, pinned to one exact version), `@phosphor-icons/react`,
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
| Record | The wire query supports refused-only, with no allowed-only filter | allowed-only is applied to each returned page, and links to older entries preserve the filters |
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
