# Importers

How history and metadata move into bee and Beehive. The contract is SPEC §17; the git
importer is `crates/bee-import` (W9); this file describes the GitHub importer,
`crates/bee-github` (W17). Other forges (GitLab, Bitbucket, Azure DevOps, Gitea) reuse the
same pipeline with their own source adapters and are not built yet.

## Who imports

- On the server, `ImportService.StartImport` needs an administrator of the target org. The import
  itself runs as the org's **importer service principal** (`importer_principal` namespace, created
  on the first import, owned by the admin who started it): imported Meta `creator`, uploads and
  ref-log entries name it, while the import's audit entry names the starter.
- Over the API, `CreateChangeset` with `import_mode` (SPEC §17.4) is reserved to service principals
  that hold `force_update` (ref administration) and write on `/`; it keeps the supplied Meta (git
  extension included), allows parentless changesets and skips push protection, so imported history
  keeps its exact git ids.

## GitHub

### One command

```sh
bee-github import acme/api                    # resumes automatically if interrupted
bee-github import acme/api --dry-run          # counts, request estimate, ETA; fetches nothing
bee-github import acme/api --mirror-only      # git only: usable in minutes
bee-github import acme/api --archive org.tar.gz   # metadata from an org migration archive
bee-github sync acme/api [--webhook-listen :8088 --webhook-secret S]
bee-github cutover acme/api [--archive-github]
bee-github claim-token acme/api <github-user-id>
```

Every command takes `--json`; errors print `{"error": {"reason", "message"}}` with a SPEC
§12.2 reason. The binary is `bee-github` because `bee-cli` and `bee-github` share
layer 6 and cannot depend on each other; `bee import github` can be a two-line wrapper
that runs it.

**Credentials.** `--github-token`; or a GitHub App (`--app-id`, `--app-key` PEM,
`--installation`), whose JWT is exchanged for an installation token and refreshed before
it expires (and once more on a `401`); otherwise `GH_TOKEN`, `GITHUB_TOKEN`, then
`gh auth token`. Git gets the token through `GIT_CONFIG_*` environment variables
(`http.extraHeader`), never on the command line.

**Target.** `--server` (`BEE_ENDPOINT`) and `--token` (`BEE_TOKEN`). The CLI first asks
the server to run the whole import itself (`StartImport` with `source_kind = GITHUB`);
servers that answer `UNIMPLEMENTED` get the git history through `StartImport` with
`source_kind = GIT` and the local mirror path, and the metadata stays in the journal,
reported as `not_supported_by_target` until a server that hosts the forge picks it up with
`--resume`. In-process (the server, tests) the importer writes to the stores and the forge
directly ([`StoreTarget`], [`HubSink`]).

### Phases

1. **Git first.** `git fetch --prune` of `refs/heads/*`, `refs/tags/*` and `refs/pull/*` into
   a bare mirror in the state directory, then `bee-import` (exact git ids, round-trip
   verification). Re-runs fetch and import only deltas.
2. **Metadata.** From the migration archive when given (no quota), then the API in
   checkpointed steps (below).
3. **Revision commits.** Every force-push's `before` head is fetched by id while GitHub still
   serves it; PR bases and old heads are pinned as `refs/pull/<n>/base` and
   `refs/pull/<n>/rev/<k>` (bee `pull/<n>/base`, `pull/<n>/rev/<k>`) and imported. A head
   GitHub no longer serves is a `force_push_commit_unreachable` drop.
4. **Principals.** One placeholder per GitHub account (below).
5. **Records** through the `ForgeSink`, each journaled with its outcome.
6. **Report**, redirect map and generated land rules, written to the state directory.

Then **live sync** keeps the mirror current, and **cutover** ends it.

### API backfill

| step | endpoint |
|---|---|
| `repo` | `GET /repos/{o}/{r}` |
| `labels`, `milestones`, `releases` | repo-wide lists |
| `issues` | `GET /repos/{o}/{r}/issues?state=all` (PR entries give PR reaction totals) |
| `pulls` | `GET /repos/{o}/{r}/pulls?state=all` |
| `review_comments`, `issue_comments` | `GET …/pulls/comments`, `GET …/issues/comments` (repo-wide) |
| `branches`, `protection:<b>` | protected branches and their protection |
| `hooks`, `secrets`, `keys` | names only; a credential that cannot read them is reported, not fatal |
| `timeline:<n>` | one GraphQL query per 25 PRs: `HeadRefForcePushedEvent`s and reviews |
| `reactions:<subject>` | per-user reactions, only for subjects whose summary is non-zero |

Per-PR REST fan-out is what makes other importers slow; batching timelines and reviews in
GraphQL keeps a Kubernetes-sized repository (92k PRs) at a few thousand GraphQL calls plus
the repo-wide lists.

**Rate limits.** `x-ratelimit-*` headers are tracked; an exhausted budget waits for the
reset. A `403`/`429` with `Retry-After` waits that long; one naming the secondary limit
backs off from a minute, doubling. Requests are sequential. A wait longer than
`max_wait_secs` fails with `RESOURCE_EXHAUSTED` and `retry_after_secs`, and the import
resumes later. Polling uses `If-None-Match`; a `304` costs no quota.

**Resume.** `<state>/journal.jsonl` gets every fetched page together with the next page's
URL, every finished step, and every placed record with a fingerprint and its outcomes. A
killed run replays the journal: pages merge again (keyed maps make that idempotent), steps
resume at their cursor, and records are placed again only when their fingerprint changed.
A line cut short by a crash is ignored. `<state>/state.json` holds the phase, the claim
secret, claims, the live-sync cursor and ETags.

### Org migration archive

`--archive` reads a `.tar.gz` from `POST /orgs/{org}/migrations` (helpers start one, poll
it and download it through GitHub's redirect). The reader takes the numbered JSON arrays
(`repositories_*`, `users_*`, `pull_requests_*`, `issues_*`, `issue_comments_*`,
`pull_request_reviews_*`, `pull_request_review_comments_*`, `releases_*`, `milestones_*`,
`protected_branches_*`) and resolves the URLs records use to refer to each other. Accounts
in an archive have logins only; the API resolves them to numeric ids (`GET /users/{login}`),
and offline the placeholder is keyed by login (a reported, synthetic id with the top bit
set). The archive's bare repository, when present, is the git source, so commit ids come
from the archive itself. The API still fills force-push timelines and reactions. The tar
reader handles ustar, GNU long names and pax paths and never writes outside the state
directory.

### Mapping

| GitHub | Beehive (`HubSink`) |
|---|---|
| PR | change request with the same number: the number is reserved in the shared per-repo number space before `Changes::open`. An open PR targets its base branch; a PR whose commits already reached the target is anchored on `pull/<n>/base` |
| pushes, force-pushes | revisions: each force-push's `before` head ends one; the current head is the last |
| merged PR | closed CR whose close reason names the merge commit (reported as archived: landing it again would rewrite history) |
| review | `submit_review` on the revision whose head it reviewed; dismissed → comment review (archived); self-approval → quoted conversation comment (archived) |
| review comment | `comment_on_lines` at the original commit's revision, line and side; replies with `reply`; vanished lines → file comment (archived); unusable anchor → conversation comment with the diff hunk (archived) |
| conversation comment, reactions | `comment`, `react`, `react_comment`; reactions on review comments have no store yet (dropped, `not_supported_by_target`) |
| issue | `Work::import_issue` (number, times, comments, labels, lock) and `add_reaction` |
| labels, milestones | `create_label`, `create_milestone` (milestone numbers are Beehive's; a change is reported) |
| release, assets | `Releases::create`, `begin_asset` + upload to the signed URL + `finalize_asset` |
| LFS objects | fetched through the LFS batch API and verified; the forge has no LFS store yet (dropped, `not_supported_by_target`) |
| branch protection, CODEOWNERS | `<state>/policy.toml` for review (below) |
| webhooks | created disabled with the GitHub payload format; secrets go on the checklist |
| Actions workflows | kept as-is in git; `bee-workflow` runs them (reported per file) |
| secrets, deploy keys | names on the manual checklist (archived) |
| wiki | `<repo>.wiki` git repository through the git target |
| users | placeholder principals (below) |

Every placed body carries `<!-- bee-github <kind> <id> by @login at <time> -->`, plus the
original text when link rewriting changed it, so placing a record twice (resume, live sync,
a fresh journal against the same forge) adds nothing.

**Links.** `https://github.com/<owner>/<repo>` in imported text becomes the Beehive
repository URL; other repositories are left alone. `#123` needs no rewrite because numbers
are kept.

### Land rules

| GitHub | `.beehive/policy.toml` |
|---|---|
| branch, `prefix*` pattern | `[[land]] ref` (GitHub's `*` stops at `/`, bee's prefix does not: reported) |
| required checks | `required_checks` |
| approvals, code owners | `[[land.approvals]] count`, `from = "owners"` |
| dismiss stale reviews, last-push approval | `dismiss_stale_approvals`, `require_last_push_approval` |
| signed commits, linear history | `require_signed`, `require_linear` |
| push restrictions, lock | `allow` (placeholder ids, `@org/team`), `allow = []` |
| deletion and creation blocked | `[[refs]] delete = []`, `create = []` |

Reported as untranslatable: force pushes allowed, admins exempt, conversation resolution,
required deployments, app push restrictions, dismissal and bypass allowances, other globs,
and CODEOWNERS lines bee's parser refuses. The output is validated with `bee-policy`.

### Principals and claims

A placeholder's id is ULID(time 0, BLAKE3(`"swarm-github-user\0"` ‖ GitHub id)[..10]): the
same person has the same placeholder in every imported repository, so one claim covers all
of them. Bots become agent principals. A claim needs a GitHub login the server verified for
that id, or the claim token (HMAC-SHA256 of the id under the state's secret, never in a
report). `Importer::claim` records it and asks the sink to reassign.

### Live sync and cutover

`POST /webhooks/github` (`live::webhook_router`) verifies `X-Hub-Signature-256`, ignores
repeated `X-GitHub-Delivery` ids, journals the object from the payload and places what it
touches; pushes refetch git. Polling (`sync`) uses `since` cursors over issues and both
comment lists, refetches the PRs they name and their timelines, and imports git deltas.
Deletions on GitHub after the import are not mirrored.

`cutover` freezes the import (webhooks then answer `409`, runs refuse), optionally archives
the GitHub repository, runs a final sync, writes `redirects.json` (every PR, issue and
release, plus a prefix rule for the rest of the repository) and the final report.

### Fidelity report

The git importer's report (`schemas/v1/import-report.schema.json`) gains the categories
`pull_requests`, `pr_revisions`, `force_push_commits`, `reviews`, `review_comments`,
`issue_comments`, `issues`, `labels`, `milestones`, `reactions`, `releases`,
`release_assets`, `lfs_objects`, `branch_protection`, `codeowners`, `webhooks`,
`workflows`, `wiki`, `principals`, `secrets`, `deploy_keys`, and a `github` section
(placeholders, manual steps, untranslatable settings, redirect count,
`unexplained_drops`). Drop reasons added: `force_push_commit_unreachable`,
`missing_commit`, `number_taken`, `not_supported_by_target`, `unreadable`,
`target_rejected`, `untranslatable`, `lfs_fetch_failed`, `integrity`,
`asset_fetch_failed`, `parent_dropped`. Every record ends as exactly one outcome, so
`complete` (nothing dropped, verification passed) is the only basis for "imported 100%".

### What the server wires

- `ImportService.StartImport` with `source_kind = GITHUB`: run `bee_github::Importer`
  with a `StoreTarget` over its stores (with its eager derived hook) and a `HubSink` over its
  forge, in a state directory under the data dir; serve the report from it.
- `ClaimIdentity` / `ListPlaceholderPrincipals`: `Importer::claim` with the verified GitHub
  id; the report's placeholder list.
- Mount `live::webhook_router` and run `sync` periodically for live imports.
- Set `include_pull_refs` for git imports of GitHub sources (until then the server target
  exposes pull refs as `refs/heads/pull/*`, which map to the same bee names).
- The LFS store, a tag store for releases, and reactions on review comments.
