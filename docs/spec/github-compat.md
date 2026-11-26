# GitHub compatibility gateway

`crates/bee-compat`. Unmodified GitHub clients work against Beehive: the `gh` CLI (PRODUCT
real-world check 3: `pr`, `issue`, `run`, `release`, `repo`, `api`), Octokit scripts, CI
integrations and `GITHUB_TOKEN`-style automation. Migration (SPEC §17) keeps numbers and git
ids; this gateway keeps the tools.

The gateway is a translator. Every request is authenticated with the bee token behind the
presented credential and turned into the forge's own RPCs (`RepoService`, `ChangeService`,
`IssueService`, `WorkflowService`, ...), so authorization, policy, audit, events and webhooks
are exactly those of the native API. Answers use GitHub's JSON shapes (REST API version
`2022-11-28`) and GitHub's GraphQL schema names. Git objects come from a VCS port in git ids.

## Mounting

| Path | Serves | Router |
|---|---|---|
| `/api/v3/...` | REST, GitHub Enterprise Server layout | `Compat::router` |
| `/api/graphql` | GraphQL | `Compat::router` |
| `/api/uploads/...` | release asset uploads (`upload_url`) | `Compat::router` |
| `/user`, `/repos/...`, `/graphql`, ... | the same REST and GraphQL at the root: the `api.<host>/` layout `gh` and Octokit use | `Compat::root_router` |

The root router claims only GitHub's top-level API segments (`user`, `users`, `repos`, `orgs`,
`organizations`, `search`, `gists`, `notifications`, `rate_limit`, `meta`, `teams`,
`installation`, `emojis`, `octocat`, `zen`, `licenses`, `markdown`, `feeds`, `events`,
`repositories`, `graphql`), so it merges with the web UI and the git gateway.

`bee-server` mounts both routers on its listener when the forge is open
(`crates/bee-server/src/forge/http.rs::compat_router`).

**Pointing `gh` at Beehive.** For a TLS host, `GH_HOST=bee.example.com` and
`GH_ENTERPRISE_TOKEN=<token>`: `gh` uses `https://<host>/api/v3/` and `/api/graphql`. For local
use, `GH_HOST=github.localhost` makes `gh` speak plain HTTP to `http://api.github.localhost/`
and `/graphql`; an `HTTP_PROXY` naming the server delivers those requests (this is what the
`BEE_TEST_GH` test does). URLs in responses are built from `public_url` when configured,
else from `X-Forwarded-Host`/`Host` and `X-Forwarded-Proto`; `html_url`s use `web_url`, else
the API origin without a leading `api.`.

## Ports

| Port | Answers | Server (`bee-server/src/forge/compat.rs`) | Test / dev implementation |
|---|---|---|---|
| `GitPort` | refs, commits, trees, blobs, paths, comparisons, diffs; git-data writes; git id ↔ changeset | `memory::HistoryGit` with `ServerRefMover`: every ref move is a `RefService` call with the caller's token | `memory::HistoryGit` |
| `RefMover` | ref moves of the git data and contents APIs | `ServerRefMover` (policy, ref events, CI, webhooks) | none: a plain compare-and-swap |
| `TokenMap` | presented credential → bee token | `PassThroughTokens` (server tokens are Biscuits) | same |
| `CompatState` | gateway-only documents | `KvState`: the server KV, namespace `compat_state` | `memory::MemoryState` |
| `AssetStore` | release asset bytes at the forge's signed upload URL | `ServerAssets`: verifies the signed URL, stores the bytes in the blob store | `memory::MemoryAssets` |
| `CodeSearch` | `GET /search/code` | `ServerCodeSearch`: the server's code index (`SearchService`), one indexing pass before each query | `memory::GrepSearch` |
| clock | rate-limit windows | the server clock | `ManualClock` |

`memory::HistoryGit` exports changesets with `bee-git`'s exporter, so every id is the one
`git clone` through the git gateway sees: imported commits keep their original ids, new
history gets the deterministic representation with its `Change-Id` trailer. `memory::HistoryTags`
(the forge's tag port over `tags/<name>` refs) and `memory::HistoryResolver` (the forge's
revision resolver over SPEC revision syntax) complete an in-process forge for tests.

**Server wiring.** The server mounts the gateway with every port above and:

- `CompatConfig.public_url` and `web_url` are `server.public_url` (derived from each request's
  `Host` when unset); `secret_box_key` is derived from the server's URL key
  (`blobs.url_key_hex` or `<data>/url.key`), never the fixed development key.
- `/releases/<repo>/<release>/<asset>?expires=&signature=` serves the forge's signed release
  asset URLs: `PUT` stores the bytes, `GET`/`HEAD` serves them; an invalid or expired
  signature answers `403` (upload) or `404` (download).
- Code search covers only paths every reader of the repo may read.
- PR head branch names are stored in the forge (`ChangeRequest.head_ref`); `CompatState` keeps
  the name for CRs opened before that field existed.

## Authentication

`Authorization: token <t>`, `Authorization: Bearer <t>` and `Authorization: Basic
base64(user:<t>)` (the password, or the user when the password is empty) are accepted. The
`TokenMap` port turns the presented token into a bee token, which the forge verifies
(signature, expiry, revocation) exactly as for native RPCs. Without credentials the gateway
asks `TokenMap::anonymous()`; when that is `None` (the default) every endpoint that reaches
the forge answers `401 Requires authentication`. An unverifiable credential answers
`401 Bad credentials`. `X-OAuth-Scopes` lists every classic scope the gateway understands;
what a token may actually do is its bee scope and the caller's roles.

## Versions, rate limits, ETags, pagination

- **API version.** `X-GitHub-Api-Version: 2022-11-28` or no header; any other value answers
  `400 API version <v> is not supported.` Every response carries
  `X-GitHub-Api-Version-Selected: 2022-11-28`, `X-GitHub-Media-Type: github.v3; format=json` and
  `X-GitHub-Request-Id`.
- **Rate limits.** Fixed one-hour windows per delegation root (the human or service an agent
  acts for), per resource: `core` (default 5000), `search` (30), `graphql` (5000); clients
  without credentials share a window per address (60). Every response carries
  `X-RateLimit-Limit`, `-Remaining`, `-Used`, `-Reset` and `-Resource`. An exhausted window
  answers `403 API rate limit exceeded for ...` with `Retry-After`. `GET /rate_limit` is free and
  reports `core`, `search`, `graphql` and GitHub's other resource names. These limits sit in
  front of the server's own admission control and per-root quotas (SPEC §15.4), which still
  apply.
- **ETags.** Every `200` JSON answer to `GET`/`HEAD` carries a weak ETag over its body;
  `If-None-Match` with a matching tag (or `*`) answers `304` with an empty body, and a `304`
  does not count against the rate limit.
- **Pagination.** `page` (from 1) and `per_page` (default 30, at most 100). Lists carry a
  `Link` header with `next`, `last`, `prev` and `first` as GitHub's do. A list is gathered from
  every forge page (at most 10,000 items) and cut locally, so `last` is exact; the commit
  list walks history lazily and carries `next` but no `last`. Actions, check and search lists
  use GitHub's `{"total_count": n, "<key>": [...]}` wrappers.

## Errors

Bodies are GitHub's `{"message", "documentation_url", "errors"?, "status"}` plus
`"bee_reason"`, the SPEC §12.2 reason, so agents can switch on the contract reason.

| SPEC reason | HTTP | `message` |
|---|---|---|
| `UNAUTHENTICATED` (no credential) | 401 | `Requires authentication` |
| `UNAUTHENTICATED`, `TOKEN_EXPIRED`, `TOKEN_REVOKED` | 401 | `Bad credentials` |
| `NOT_FOUND`, `REDACTED` (and anything the caller may not read) | 404 | `Not Found` |
| `PERMISSION_DENIED`, `POLICY_DENIED`, `BUDGET_EXHAUSTED`, `QUOTA_EXCEEDED`, `LOCKED`, `BLOCKED`, `SSO_REQUIRED`, ... | 403 | the forge's message |
| `ARCHIVED` | 403 | `Repository was archived so is read-only.` |
| `RESOURCE_EXHAUSTED` | 429 | the forge's message, with `Retry-After` |
| `INVALID_ARGUMENT`, `TOO_LARGE`, `AMBIGUOUS_PREFIX` | 422 | `Validation Failed`, `errors: [{code: <subcode or "invalid">, message}]` |
| `ALREADY_EXISTS` | 422 | `Validation Failed`, `errors: [{code: "already_exists"}]` |
| `VERSION_MISMATCH`, `LEASED` | 409 | the forge's message |
| `MERGE_CONFLICT`, `ALREADY_LANDED`, `EMPTY_CHANGE`, `MERGE_IN_STACK`, `DRAFT` | 405 | the forge's message |
| `NOT_FAST_FORWARD` | 422 | `Update is not a fast forward` |
| `UNAVAILABLE`, `LAND_CONTENTION` | 503 | the forge's message |
| `DEADLINE_EXCEEDED` | 504 | the forge's message |
| `UNIMPLEMENTED` | 501 | the forge's message |

Endpoint-specific statuses follow GitHub: a refused merge (policy, missing approvals or
checks) is `405`, a stale `sha` on merge is `409 Head branch was modified`, a missing `sha` on a
contents update is `422 "sha" wasn't supplied`, a stale one `409`.

## Ids

GitHub clients expect integer ids that fit a JavaScript number. Every object gets
`id = BLAKE3("swarm-compat id v1\0" ‖ kind ‖ "\0" ‖ bee id)` truncated to 53 bits: a pure
function of the object, stable across restarts and replicas, equal between REST `id` and
GraphQL `databaseId`. Paths that take a numeric id resolve it through an in-memory index filled
as objects are rendered, then by scanning the parent collection. GraphQL node ids are
`<prefix>_<base64url(kind ":" key)>` (keys: repo id, `repo_id:number` for issues, PRs and
milestones, `repo_id:name` for labels, principal id for users), decodable without state.
Gists keep their forge ids; notification thread ids are the numeric form as strings.

## Mapping

| GitHub | Beehive |
|---|---|
| owner | the org (`repo.full_name` is `org/name`); personal namespaces are orgs named after the login |
| user `login` | the principal's first login in the directory, else its id; agents and services are `type: "Bot"` |
| pull request | change request with the **same number** (numbers are shared with issues, as on GitHub); `merged` = landed |
| PR `head.ref` | the CR's `head_ref` (the branch the PR was opened from), else `pull/<n>/head` |
| PR `head.repo`, `isCrossRepository`, `maintainer_can_modify` | the CR's `head_repo_id` (a fork) and `allow_maintainer_edits` |
| PR `head.sha` / `base.sha` | git ids of the current revision's head and base |
| merge methods `merge` / `squash` / `rebase` | land methods `MERGE_COMMIT` / `SQUASH` / `REBASE` |
| reviews `APPROVE` / `REQUEST_CHANGES` / `COMMENT` | verdicts; dismissed reviews are `DISMISSED` |
| review comments and replies | review threads anchored to lines (`side` `LEFT`/`RIGHT` ↔ `old`/`new`) |
| issues API on a PR number | the CR: labels, assignees, milestone, lock, reactions, conversation comments |
| branch protection | the `[[land]]` rule for the branch in `.beehive/policy.toml` on the default branch (required checks, approvals and code-owner approvals, stale dismissal, last-push approval, linear history, signatures, lock) |
| refs `refs/heads/x`, `refs/tags/x`, `refs/pull/x`, `refs/x` | bee refs `x`, `tags/x`, `pull/x`, `git/x` (SPEC §17.2) |
| commit statuses | the forge's statuses (each also mirrored as a check run named after the context; check lists hide those mirrors) |
| check runs and suites | check runs and suites (`status`/`conclusion` ↔ check status) |
| webhook `events` | forge event kinds whose GitHub name matches, through the module registry; hooks created here use the GitHub payload format |
| Actions secrets | libsodium sealed boxes opened with the gateway's X25519 key, then stored by the forge's secret store (sealed again with the server key) |
| workflows, runs, jobs, artifacts | `WorkflowService`; logs and artifact downloads redirect to the forge's signed URLs |
| releases and assets | `ReleaseService`; asset bytes go to the signed upload URL through `AssetStore` |

## GraphQL

`POST /api/graphql` runs a hand-written executor (`src/graphql/`) over a schema subset written
in SDL (`src/graphql/schema.graphql`): queries and mutations with variables and defaults,
aliases, fragments and inline fragments on interfaces and unions, `@include`/`@skip`,
`__typename`, and introspection (`__type`, `__schema`) generated from the SDL. GHES-mode `gh`
uses introspection for feature detection. An in-crate executor was chosen over `async-graphql`:
the gateway needs one dynamic object model over the forge, which is about a thousand lines here,
while `async-graphql` would add some thirty crates not in the lockfile and a derive-based schema
that would duplicate every GitHub type anyway.

The schema covers what `gh pr`, `gh issue` and `gh repo` query (`Repository`, `PullRequest`,
`Issue`, `IssueComment`, `PullRequestReview`, `ReviewRequest`, `Commit`, `StatusCheckRollup`,
`CheckRun`, `StatusContext`, `Ref`, `BranchProtectionRule`, `Release`, `Label`, `Milestone`,
`User`, `Bot`, `Organization`, `Team`, connections with `totalCount`/`pageInfo`, and `search`
for `ISSUE` and `REPOSITORY`) and the mutations they send (`createPullRequest`,
`updatePullRequest`, `mergePullRequest`, `closePullRequest`, `reopenPullRequest`,
`markPullRequestReadyForReview`, `convertPullRequestToDraft`, `requestReviews`,
`addPullRequestReview`, `enablePullRequestAutoMerge`, `disablePullRequestAutoMerge`,
`updatePullRequestBranch`, `addComment`, `createIssue`, `updateIssue`, `closeIssue`,
`reopenIssue`, `deleteIssue`, `addLabelsToLabelable`, `removeLabelsFromLabelable`,
`addAssigneesToAssignable`, `removeAssigneesFromAssignable`, `addStar`, `removeStar`,
`addReaction`, `lockLockable`, `unlockLockable`). A field outside the schema is an error
(`type: "UNDEFINED_FIELD"`, the field `null`), as on GitHub; resolver failures carry GitHub's
`type` (`NOT_FOUND`, `FORBIDDEN`, `UNPROCESSABLE`, ...) and `path`. Requests count one point
against the `graphql` rate limit.

## `gh` commands verified

Run by `crates/bee-compat/tests/gh_cli.rs` (`BEE_TEST_GH=1`) with unmodified `gh` 2.70.0
against an in-process `bee-server --dev` with the gateway mounted:

| Command | Verified |
|---|---|
| `gh api user`, `gh api repos/<o>/<r>`, `gh api -X PUT .../contents/...`, `gh api .../git/refs -f ...` | REST through the root aliases, writes, a protected-branch refusal |
| `gh api --paginate .../labels?per_page=2` | `Link` pagination |
| `gh api graphql -f query=...` | GraphQL |
| `gh repo view <r>`, `gh repo view --json ...` | repository query and README |
| `gh issue create`, `gh issue list [--json]`, `gh issue view --json`, `gh issue comment`, `gh issue close`, `gh issue list --state closed` | issues |
| `gh pr create --head --base`, `gh pr list --json`, `gh pr view [--json]`, `gh pr status`, `gh pr review --comment`, `gh pr checks`, `gh pr merge --squash` | pull requests end to end (AS-API-01) |
| `gh release create`, `gh release list`, `gh release view --json` | releases |
| `gh run list --json`, `gh workflow list` | Actions |
| `gh secret set`, `gh secret list` | sealed boxes from Go's `nacl/box` opened by the gateway |
| `gh repo fork --clone=false`, `gh pr create --head <owner>:<branch>`, `gh pr view --json isCrossRepository,headRepositoryOwner` | forks and PRs from a fork (`crates/bee-server/tests/forks_compat.rs`, same opt-in) |

The same query shapes (`PullRequestList`, `PullRequestByNumber`, `PullRequestStatus`,
`PullRequestStatusChecks`, `RepositoryInfo`, `IssueList`, `IssueByNumber`, and the mutations)
are also replayed in `tests/compat.rs` on every `cargo test`.

## Differences from GitHub

Intentional, or bounded by what the forge serves today:

- **Protected branches move only through Land.** Contents writes, ref updates and ref
  deletions on a branch the repo policy protects answer `422` (`bee_reason:
  POLICY_DENIED`); open a pull request. A merge-commit land onto a protected ref may be refused
  by the server's lander (it lands linear stacks); use `squash` or `rebase`.
- **No pending reviews.** A review without `event` (or `PENDING`) answers `422`; reviews are
  submitted at once.
- **PR head branches.** A PR's head branch is the CR's `head_ref`, set when the PR is opened
  through the gateway (or natively with `head_ref`); otherwise it shows `pull/<n>/head`.
  Pushing to the branch of a same-repo PR does not revise it by itself (the git gateway's
  `push.autoChange` or `ReviseChange` does); pushing to the fork branch of a PR from a fork
  does.
- **Converting an issue into a PR** (`issue` on `POST /pulls`) is refused.
- **Editing or deleting PR conversation comments** through the issues comments API answers
  `422`; issue comments can be edited and deleted.
- **Deployments.** A `ref` that is a commit id deploys that commit from the default branch;
  required contexts are checked by the forge (a failure answers `409`). Protection rules and
  reviews come from the environment.
- **Forks** (`docs/spec/forks.md`). `POST /forks` answers `202` with the fork at once (it is
  complete when returned); `organization`, `name` and `default_branch_only` are honoured, and
  asking again for a namespace that already has a fork returns it. A PR from a fork is
  `head: "owner:branch"` (or `head_repo`, or GraphQL `headRepositoryId`); the fork's stack is
  copied into the upstream when the PR is opened or revised. Fork visibility cannot be
  changed. Fork runs of first-time contributors wait for approval and get no secrets.
- **Repo creation with `auto_init` or `license_template`** depends on the server's provisioner.
- **Git data writes.** `POST /git/commits` creates a bee changeset; the returned commit is its
  git representation, so the message gains a `Change-Id` trailer and the committer is the
  caller. Submodule entries cannot be written. Blobs and trees created but never committed are
  held in memory. Ref moves (`/git/refs`, contents writes) go through `RefService`, so they
  are authorized by repo policy and start CI and webhooks like any push.
- **Commit identities.** `author`/`committer` accounts are matched by name against logins.
- **Reactions** on PRs are counts (no per-user list); issue reactions are listed per user.
- **Search.** Issue search hands GitHub qualifiers to the forge's query language; `repo:`,
  `org:`/`user:`, `is:pr`/`is:issue`/`type:` pick the targets. Repository, user, commit,
  label and topic search filter what the caller can see. Code search runs on `bee-search`
  over each repo's default ref and the paths every reader may read; `text_matches` are
  matching lines.
- **Users and orgs.** `GET /users/{org}` returns the org as an account (`type:
  "Organization"`). Followers, emails and starring use the forge's user module.
- **Releases.** Deleting a release keeps its tag, as on GitHub. Asset bytes live in the blob
  store; downloads redirect to a signed URL valid for 15 minutes.
- **Notifications.** Thread subjects link to the repository, not the issue or PR.
- **Timestamps** have second precision; `updated_at` falls back to `created_at`.
- **Not served:** OAuth app and GitHub App management, Codespaces, Packages (served by the
  registry endpoints), Projects (classic and v2 return empty connections), Discussions,
  repository traffic and statistics, Dependabot and code scanning REST, SCIM (served
  natively), and GraphQL subscriptions.

## Endpoints served

Paths are relative to `/api/v3` (and to the root through the aliases). `GET` routes also answer
`HEAD`. Anything else answers `404`.

### meta

| Method | Path |
|---|---|
| `GET` | `/` |
| `GET` | `/meta` |
| `GET` | `/rate_limit` |
| `GET` | `/zen` |
| `GET` | `/octocat` |
| `GET` | `/versions` |
| `GET` | `/emojis` |

### users

| Method | Path |
|---|---|
| `GET` | `/user` |
| `PATCH` | `/user` |
| `GET` | `/user/emails` |
| `GET` | `/user/orgs` |
| `GET` | `/user/memberships/orgs` |
| `GET` | `/user/repos` |
| `POST` | `/user/repos` |
| `GET` | `/user/starred` |
| `GET` | `/user/starred/{owner}/{repo}` |
| `PUT` | `/user/starred/{owner}/{repo}` |
| `DELETE` | `/user/starred/{owner}/{repo}` |
| `GET` | `/user/subscriptions` |
| `GET` | `/user/followers` |
| `GET` | `/user/following` |
| `GET` | `/user/following/{user}` |
| `PUT` | `/user/following/{user}` |
| `DELETE` | `/user/following/{user}` |
| `GET` | `/user/issues` |
| `GET` | `/user/teams` |
| `GET` | `/issues` |
| `GET` | `/users/{user}` |
| `GET` | `/users/{user}/orgs` |
| `GET` | `/users/{user}/repos` |
| `GET` | `/users/{user}/followers` |
| `GET` | `/users/{user}/following` |
| `GET` | `/users/{user}/starred` |
| `GET` | `/users/{user}/gists` |

### orgs, members, teams

| Method | Path |
|---|---|
| `GET` | `/organizations` |
| `GET` | `/orgs/{org}` |
| `PATCH` | `/orgs/{org}` |
| `GET` | `/orgs/{org}/repos` |
| `POST` | `/orgs/{org}/repos` |
| `GET` | `/orgs/{org}/members` |
| `GET` | `/orgs/{org}/members/{user}` |
| `DELETE` | `/orgs/{org}/members/{user}` |
| `GET` | `/orgs/{org}/memberships/{user}` |
| `PUT` | `/orgs/{org}/memberships/{user}` |
| `DELETE` | `/orgs/{org}/memberships/{user}` |
| `GET` | `/orgs/{org}/public_members` |
| `GET` | `/orgs/{org}/outside_collaborators` |
| `GET` | `/orgs/{org}/teams` |
| `POST` | `/orgs/{org}/teams` |
| `GET` | `/orgs/{org}/teams/{slug}` |
| `PATCH` | `/orgs/{org}/teams/{slug}` |
| `DELETE` | `/orgs/{org}/teams/{slug}` |
| `GET` | `/orgs/{org}/teams/{slug}/members` |
| `GET` | `/orgs/{org}/teams/{slug}/memberships/{user}` |
| `PUT` | `/orgs/{org}/teams/{slug}/memberships/{user}` |
| `DELETE` | `/orgs/{org}/teams/{slug}/memberships/{user}` |
| `GET` | `/orgs/{org}/teams/{slug}/repos` |
| `PUT` | `/orgs/{org}/teams/{slug}/repos/{owner}/{repo}` |
| `DELETE` | `/orgs/{org}/teams/{slug}/repos/{owner}/{repo}` |
| `GET` | `/orgs/{org}/hooks` |
| `POST` | `/orgs/{org}/hooks` |
| `GET` | `/orgs/{org}/hooks/{id}` |
| `PATCH` | `/orgs/{org}/hooks/{id}` |
| `DELETE` | `/orgs/{org}/hooks/{id}` |
| `POST` | `/orgs/{org}/hooks/{id}/pings` |
| `GET` | `/orgs/{org}/hooks/{id}/deliveries` |
| `GET` | `/orgs/{org}/actions/secrets/public-key` |
| `GET` | `/orgs/{org}/actions/secrets` |
| `GET` | `/orgs/{org}/actions/secrets/{name}` |
| `PUT` | `/orgs/{org}/actions/secrets/{name}` |
| `DELETE` | `/orgs/{org}/actions/secrets/{name}` |
| `GET` | `/orgs/{org}/actions/variables` |
| `POST` | `/orgs/{org}/actions/variables` |
| `GET` | `/orgs/{org}/actions/variables/{name}` |
| `PATCH` | `/orgs/{org}/actions/variables/{name}` |
| `DELETE` | `/orgs/{org}/actions/variables/{name}` |

### search

| Method | Path |
|---|---|
| `GET` | `/search/issues` |
| `GET` | `/search/repositories` |
| `GET` | `/search/code` |
| `GET` | `/search/users` |
| `GET` | `/search/commits` |
| `GET` | `/search/labels` |
| `GET` | `/search/topics` |

### notifications

| Method | Path |
|---|---|
| `GET` | `/notifications` |
| `PUT` | `/notifications` |
| `GET` | `/notifications/threads/{id}` |
| `PATCH` | `/notifications/threads/{id}` |
| `DELETE` | `/notifications/threads/{id}` |
| `GET` | `/notifications/threads/{id}/subscription` |
| `PUT` | `/notifications/threads/{id}/subscription` |
| `DELETE` | `/notifications/threads/{id}/subscription` |

### gists

| Method | Path |
|---|---|
| `GET` | `/gists` |
| `POST` | `/gists` |
| `GET` | `/gists/public` |
| `GET` | `/gists/starred` |
| `GET` | `/gists/{id}` |
| `PATCH` | `/gists/{id}` |
| `DELETE` | `/gists/{id}` |
| `GET` | `/gists/{id}/commits` |
| `GET` | `/gists/{id}/forks` |
| `POST` | `/gists/{id}/forks` |
| `GET` | `/gists/{id}/star` |
| `PUT` | `/gists/{id}/star` |
| `DELETE` | `/gists/{id}/star` |
| `GET` | `/gists/{id}/comments` |
| `POST` | `/gists/{id}/comments` |
| `GET` | `/gists/{id}/{sha}` |

### repos

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}` |
| `PATCH` | `/repos/{owner}/{repo}` |
| `DELETE` | `/repos/{owner}/{repo}` |
| `GET` | `/repos/{owner}/{repo}/forks` |
| `POST` | `/repos/{owner}/{repo}/forks` |
| `GET` | `/repos/{owner}/{repo}/topics` |
| `PUT` | `/repos/{owner}/{repo}/topics` |
| `GET` | `/repos/{owner}/{repo}/languages` |
| `GET` | `/repos/{owner}/{repo}/contributors` |
| `GET` | `/repos/{owner}/{repo}/tags` |
| `POST` | `/repos/{owner}/{repo}/dispatches` |
| `POST` | `/repos/{owner}/{repo}/transfer` |
| `GET` | `/repos/{owner}/{repo}/collaborators` |
| `GET` | `/repos/{owner}/{repo}/collaborators/{user}` |
| `PUT` | `/repos/{owner}/{repo}/collaborators/{user}` |
| `DELETE` | `/repos/{owner}/{repo}/collaborators/{user}` |
| `GET` | `/repos/{owner}/{repo}/collaborators/{user}/permission` |
| `GET` | `/repos/{owner}/{repo}/teams` |
| `GET` | `/repos/{owner}/{repo}/stargazers` |
| `GET` | `/repos/{owner}/{repo}/subscribers` |
| `GET` | `/repos/{owner}/{repo}/subscription` |
| `PUT` | `/repos/{owner}/{repo}/subscription` |
| `DELETE` | `/repos/{owner}/{repo}/subscription` |
| `GET` | `/repos/{owner}/{repo}/notifications` |
| `GET` | `/repos/{owner}/{repo}/events` |

### contents and git data

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/readme` |
| `GET` | `/repos/{owner}/{repo}/readme/{dir}` |
| `GET` | `/repos/{owner}/{repo}/contents` |
| `GET` | `/repos/{owner}/{repo}/contents/{path}` |
| `PUT` | `/repos/{owner}/{repo}/contents/{path}` |
| `DELETE` | `/repos/{owner}/{repo}/contents/{path}` |
| `GET` | `/repos/{owner}/{repo}/tarball` |
| `GET` | `/repos/{owner}/{repo}/tarball/{ref}` |
| `GET` | `/repos/{owner}/{repo}/zipball` |
| `GET` | `/repos/{owner}/{repo}/zipball/{ref}` |
| `GET` | `/repos/{owner}/{repo}/git/refs` |
| `GET` | `/repos/{owner}/{repo}/git/refs/{ref}` |
| `GET` | `/repos/{owner}/{repo}/git/matching-refs/{ref}` |
| `GET` | `/repos/{owner}/{repo}/git/ref/{ref}` |
| `POST` | `/repos/{owner}/{repo}/git/refs` |
| `PATCH` | `/repos/{owner}/{repo}/git/refs/{ref}` |
| `DELETE` | `/repos/{owner}/{repo}/git/refs/{ref}` |
| `GET` | `/repos/{owner}/{repo}/git/commits/{sha}` |
| `POST` | `/repos/{owner}/{repo}/git/commits` |
| `GET` | `/repos/{owner}/{repo}/git/trees/{sha}` |
| `POST` | `/repos/{owner}/{repo}/git/trees` |
| `GET` | `/repos/{owner}/{repo}/git/blobs/{sha}` |
| `POST` | `/repos/{owner}/{repo}/git/blobs` |
| `GET` | `/repos/{owner}/{repo}/branches` |
| `GET` | `/repos/{owner}/{repo}/branches/{rest}` |
| `PUT` | `/repos/{owner}/{repo}/branches/{rest}` |
| `DELETE` | `/repos/{owner}/{repo}/branches/{rest}` |
| `POST` | `/repos/{owner}/{repo}/branches/{rest}` |
| `GET` | `/repos/{owner}/{repo}/commits` |
| `GET` | `/repos/{owner}/{repo}/commits/{ref}/status` |
| `GET` | `/repos/{owner}/{repo}/commits/{ref}/statuses` |
| `GET` | `/repos/{owner}/{repo}/commits/{ref}/check-runs` |
| `GET` | `/repos/{owner}/{repo}/commits/{ref}/check-suites` |
| `GET` | `/repos/{owner}/{repo}/commits/{ref}/pulls` |
| `GET` | `/repos/{owner}/{repo}/commits/{ref}` |
| `GET` | `/repos/{owner}/{repo}/compare/{basehead}` |

### statuses and checks

| Method | Path |
|---|---|
| `POST` | `/repos/{owner}/{repo}/statuses/{sha}` |
| `GET` | `/repos/{owner}/{repo}/statuses/{sha}` |
| `POST` | `/repos/{owner}/{repo}/check-runs` |
| `GET` | `/repos/{owner}/{repo}/check-runs/{id}` |
| `PATCH` | `/repos/{owner}/{repo}/check-runs/{id}` |
| `GET` | `/repos/{owner}/{repo}/check-runs/{id}/annotations` |
| `POST` | `/repos/{owner}/{repo}/check-runs/{id}/rerequest` |
| `POST` | `/repos/{owner}/{repo}/check-suites` |
| `GET` | `/repos/{owner}/{repo}/check-suites/{id}` |
| `GET` | `/repos/{owner}/{repo}/check-suites/{id}/check-runs` |
| `POST` | `/repos/{owner}/{repo}/check-suites/{id}/rerequest` |

### deployments and environments

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/deployments` |
| `GET` | `/repos/{owner}/{repo}/deployments/{id}` |
| `GET` | `/repos/{owner}/{repo}/deployments/{id}/statuses` |
| `POST` | `/repos/{owner}/{repo}/deployments` |
| `DELETE` | `/repos/{owner}/{repo}/deployments/{id}` |
| `POST` | `/repos/{owner}/{repo}/deployments/{id}/statuses` |
| `GET` | `/repos/{owner}/{repo}/deployments/{id}/statuses/{status}` |
| `GET` | `/repos/{owner}/{repo}/environments` |
| `GET` | `/repos/{owner}/{repo}/environments/{name}` |
| `PUT` | `/repos/{owner}/{repo}/environments/{name}` |
| `DELETE` | `/repos/{owner}/{repo}/environments/{name}` |

### hooks

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/hooks` |
| `POST` | `/repos/{owner}/{repo}/hooks` |
| `GET` | `/repos/{owner}/{repo}/hooks/{id}` |
| `PATCH` | `/repos/{owner}/{repo}/hooks/{id}` |
| `DELETE` | `/repos/{owner}/{repo}/hooks/{id}` |
| `POST` | `/repos/{owner}/{repo}/hooks/{id}/pings` |
| `POST` | `/repos/{owner}/{repo}/hooks/{id}/tests` |
| `GET` | `/repos/{owner}/{repo}/hooks/{id}/config` |
| `PATCH` | `/repos/{owner}/{repo}/hooks/{id}/config` |
| `GET` | `/repos/{owner}/{repo}/hooks/{id}/deliveries` |
| `GET` | `/repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery}` |
| `POST` | `/repos/{owner}/{repo}/hooks/{id}/deliveries/{delivery}/attempts` |

### pulls

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/pulls` |
| `POST` | `/repos/{owner}/{repo}/pulls` |
| `GET` | `/repos/{owner}/{repo}/pulls/comments` |
| `GET` | `/repos/{owner}/{repo}/pulls/comments/{id}` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}` |
| `PATCH` | `/repos/{owner}/{repo}/pulls/{number}` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/commits` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/files` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/merge` |
| `PUT` | `/repos/{owner}/{repo}/pulls/{number}/merge` |
| `PUT` | `/repos/{owner}/{repo}/pulls/{number}/update-branch` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/reviews` |
| `POST` | `/repos/{owner}/{repo}/pulls/{number}/reviews` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/reviews/{id}` |
| `PUT` | `/repos/{owner}/{repo}/pulls/{number}/reviews/{id}/dismissals` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/reviews/{id}/comments` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/comments` |
| `POST` | `/repos/{owner}/{repo}/pulls/{number}/comments` |
| `POST` | `/repos/{owner}/{repo}/pulls/{number}/comments/{id}/replies` |
| `GET` | `/repos/{owner}/{repo}/pulls/{number}/requested_reviewers` |
| `POST` | `/repos/{owner}/{repo}/pulls/{number}/requested_reviewers` |
| `DELETE` | `/repos/{owner}/{repo}/pulls/{number}/requested_reviewers` |

### issues, labels, milestones

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/issues` |
| `POST` | `/repos/{owner}/{repo}/issues` |
| `GET` | `/repos/{owner}/{repo}/issues/comments` |
| `GET` | `/repos/{owner}/{repo}/issues/comments/{id}` |
| `PATCH` | `/repos/{owner}/{repo}/issues/comments/{id}` |
| `DELETE` | `/repos/{owner}/{repo}/issues/comments/{id}` |
| `GET` | `/repos/{owner}/{repo}/issues/comments/{id}/reactions` |
| `POST` | `/repos/{owner}/{repo}/issues/comments/{id}/reactions` |
| `GET` | `/repos/{owner}/{repo}/issues/events` |
| `GET` | `/repos/{owner}/{repo}/issues/{number}` |
| `PATCH` | `/repos/{owner}/{repo}/issues/{number}` |
| `PUT` | `/repos/{owner}/{repo}/issues/{number}/lock` |
| `DELETE` | `/repos/{owner}/{repo}/issues/{number}/lock` |
| `GET` | `/repos/{owner}/{repo}/issues/{number}/comments` |
| `POST` | `/repos/{owner}/{repo}/issues/{number}/comments` |
| `GET` | `/repos/{owner}/{repo}/issues/{number}/labels` |
| `POST` | `/repos/{owner}/{repo}/issues/{number}/labels` |
| `PUT` | `/repos/{owner}/{repo}/issues/{number}/labels` |
| `DELETE` | `/repos/{owner}/{repo}/issues/{number}/labels` |
| `DELETE` | `/repos/{owner}/{repo}/issues/{number}/labels/{name}` |
| `POST` | `/repos/{owner}/{repo}/issues/{number}/assignees` |
| `DELETE` | `/repos/{owner}/{repo}/issues/{number}/assignees` |
| `GET` | `/repos/{owner}/{repo}/issues/{number}/events` |
| `GET` | `/repos/{owner}/{repo}/issues/{number}/timeline` |
| `GET` | `/repos/{owner}/{repo}/issues/{number}/reactions` |
| `POST` | `/repos/{owner}/{repo}/issues/{number}/reactions` |
| `GET` | `/repos/{owner}/{repo}/assignees` |
| `GET` | `/repos/{owner}/{repo}/assignees/{user}` |
| `GET` | `/repos/{owner}/{repo}/labels` |
| `POST` | `/repos/{owner}/{repo}/labels` |
| `GET` | `/repos/{owner}/{repo}/labels/{name}` |
| `PATCH` | `/repos/{owner}/{repo}/labels/{name}` |
| `DELETE` | `/repos/{owner}/{repo}/labels/{name}` |
| `GET` | `/repos/{owner}/{repo}/milestones` |
| `POST` | `/repos/{owner}/{repo}/milestones` |
| `GET` | `/repos/{owner}/{repo}/milestones/{number}` |
| `PATCH` | `/repos/{owner}/{repo}/milestones/{number}` |
| `DELETE` | `/repos/{owner}/{repo}/milestones/{number}` |

### releases

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/releases` |
| `POST` | `/repos/{owner}/{repo}/releases` |
| `GET` | `/repos/{owner}/{repo}/releases/latest` |
| `GET` | `/repos/{owner}/{repo}/releases/tags/{tag}` |
| `POST` | `/repos/{owner}/{repo}/releases/generate-notes` |
| `GET` | `/repos/{owner}/{repo}/releases/assets/{id}` |
| `PATCH` | `/repos/{owner}/{repo}/releases/assets/{id}` |
| `DELETE` | `/repos/{owner}/{repo}/releases/assets/{id}` |
| `GET` | `/repos/{owner}/{repo}/releases/{id}` |
| `PATCH` | `/repos/{owner}/{repo}/releases/{id}` |
| `DELETE` | `/repos/{owner}/{repo}/releases/{id}` |
| `GET` | `/repos/{owner}/{repo}/releases/{id}/assets` |

### actions

| Method | Path |
|---|---|
| `GET` | `/repos/{owner}/{repo}/actions/workflows` |
| `GET` | `/repos/{owner}/{repo}/actions/workflows/{id}` |
| `PUT` | `/repos/{owner}/{repo}/actions/workflows/{id}/enable` |
| `PUT` | `/repos/{owner}/{repo}/actions/workflows/{id}/disable` |
| `POST` | `/repos/{owner}/{repo}/actions/workflows/{id}/dispatches` |
| `GET` | `/repos/{owner}/{repo}/actions/workflows/{id}/runs` |
| `GET` | `/repos/{owner}/{repo}/actions/runs` |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{id}` |
| `DELETE` | `/repos/{owner}/{repo}/actions/runs/{id}` |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{id}/attempts/{attempt}` |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{id}/attempts/{attempt}/jobs` |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{id}/jobs` |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{id}/logs` |
| `DELETE` | `/repos/{owner}/{repo}/actions/runs/{id}/logs` |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{id}/artifacts` |
| `POST` | `/repos/{owner}/{repo}/actions/runs/{id}/cancel` |
| `POST` | `/repos/{owner}/{repo}/actions/runs/{id}/force-cancel` |
| `POST` | `/repos/{owner}/{repo}/actions/runs/{id}/rerun` |
| `POST` | `/repos/{owner}/{repo}/actions/runs/{id}/rerun-failed-jobs` |
| `POST` | `/repos/{owner}/{repo}/actions/runs/{id}/approve` |
| `GET` | `/repos/{owner}/{repo}/actions/jobs/{id}` |
| `GET` | `/repos/{owner}/{repo}/actions/jobs/{id}/logs` |
| `POST` | `/repos/{owner}/{repo}/actions/jobs/{id}/rerun` |
| `GET` | `/repos/{owner}/{repo}/actions/artifacts` |
| `GET` | `/repos/{owner}/{repo}/actions/artifacts/{id}` |
| `DELETE` | `/repos/{owner}/{repo}/actions/artifacts/{id}` |
| `GET` | `/repos/{owner}/{repo}/actions/artifacts/{id}/{format}` |
| `GET` | `/repos/{owner}/{repo}/actions/secrets/public-key` |
| `GET` | `/repos/{owner}/{repo}/actions/secrets` |
| `GET` | `/repos/{owner}/{repo}/actions/organization-secrets` |
| `GET` | `/repos/{owner}/{repo}/actions/secrets/{name}` |
| `PUT` | `/repos/{owner}/{repo}/actions/secrets/{name}` |
| `DELETE` | `/repos/{owner}/{repo}/actions/secrets/{name}` |
| `GET` | `/repos/{owner}/{repo}/actions/variables` |
| `POST` | `/repos/{owner}/{repo}/actions/variables` |
| `GET` | `/repos/{owner}/{repo}/actions/variables/{name}` |
| `PATCH` | `/repos/{owner}/{repo}/actions/variables/{name}` |
| `DELETE` | `/repos/{owner}/{repo}/actions/variables/{name}` |
| `GET` | `/repos/{owner}/{repo}/environments/{env}/secrets/public-key` |
| `GET` | `/repos/{owner}/{repo}/environments/{env}/secrets` |
| `PUT` | `/repos/{owner}/{repo}/environments/{env}/secrets/{name}` |
| `DELETE` | `/repos/{owner}/{repo}/environments/{env}/secrets/{name}` |

### uploads

| Method | Path |
|---|---|
| `POST` | `/api/uploads/repos/{owner}/{repo}/releases/{id}/assets?name=&label=` |

### GraphQL

| Method | Path |
|---|---|
| `POST` | `/api/graphql` (and `/graphql`) |
