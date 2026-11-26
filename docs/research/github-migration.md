# Migrating off GitHub: what has to move, and how others do it

Research note behind SPEC §17. Compiled 2026-10-17 from GitHub docs, the docs of other
migrators, and live `gh api` checks. Items marked *unverified* were not confirmed against a source.

## What a GitHub repo holds

- **Git:** all refs and tags, plus read-only `refs/pull/<n>/head`, which hold PR commits that
  aren't in branch history
  (https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests/checking-out-pull-requests-locally).
  Also LFS objects (outside git), submodule gitlinks, and the wiki (a separate repo).
- **Collaboration:** PRs with reviews, review threads and timeline events; issues, labels,
  milestones, releases and assets, commit comments; Discussions and Projects v2 (GraphQL only,
  *unverified*).
- **Review comment anchors** (REST): `commit_id`, `original_commit_id`, `position`,
  `original_position`, `line`, `original_line`, `start_line`, `side`, `diff_hunk`,
  `in_reply_to_id`. `position` goes null when outdated (https://docs.github.com/en/rest/pulls/comments).
  GraphQL review threads add `isOutdated` and `isResolved`; `HeadRefForcePushedEvent` exposes
  before and after commits.
- **Config:** workflows, environments, branch protection and rulesets, CODEOWNERS, webhooks,
  deploy keys, autolinks, merge settings.
- **Not exportable:** secret values (names only, https://docs.github.com/en/rest/actions/secrets).
  GitHub Enterprise Importer also leaves out variables, environments, artifacts, packages,
  status checks, audit logs, security alerts, forks, user access and keys.

## How others migrate

| Tool | Notes |
|---|---|
| GitHub Enterprise Importer | Into GitHub Enterprise Cloud only. 40 GiB git / 40 GiB metadata limits. Non-commit activity goes to "mannequin" placeholders that members reclaim by email invitation |
| Org migrations archive API | Bulk tarball of JSON, no REST quota cost, kept 7 days; locks repos unless told not to. ghe-migrator drops LFS and file-level review comments; archives must be under 20 GB (https://docs.github.com/en/rest/migrations/orgs) |
| GitLab importer | Brings PRs, reviews, wiki, protections, LFS. Approvals become comments; status checks and old diffs are lost. Kubernetes (80k PRs, 45k issues, 1.5M comments) took **76 hours**. Placeholder users reassigned by email (https://docs.gitlab.com/user/project/import/github/) |
| Gitea / Forgejo | Issues, PRs, reviews, releases, wiki; long history of rate-limit fixes and missed comments. Mirrors are set at creation; Forgejo Actions is "familiar" but not compatible |
| Radicle, Tangled, Sourcehut | Partial or community tools; Tangled is just a git remote change |

What everyone loses: identity (handled with placeholders), approvals and checks, review
context after force-pushes, secrets, and CI. What makes everyone slow: per-PR REST fan-out
against hourly quotas.

## API limits for bulk export

- REST: 5,000 req/h per user; GitHub App installs 5,000–12,500 (15,000 on Enterprise Cloud);
  secondary limits of 100 concurrent requests, 900 points/min, and 80 writes/min or 500/h;
  100 items per page
  (https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api).
- GraphQL: 5,000 points/h (10,000 Enterprise Cloud), 100 per connection, 10 s timeout.
- Our estimate for 50k PRs: naive REST is ~350k requests (≈70 h on a personal token); repo-wide
  comment endpoints, batched GraphQL and the migration archive bring it to roughly 12–30 h with
  a GitHub App. The archive-first path is the fastest.

## What we decided (SPEC §17)

- One command, a GitHub App for quota, git first so the repo is usable in minutes, archive
  before API, checkpointed and resumable, live webhook sync after.
- Keep exact git commit ids by storing original commit headers; never rewrite history.
- Same PR and issue numbers, link redirects, and rewritten references with originals kept.
- Claimable placeholder identities keyed by GitHub user id, claimed with GitHub login and
  applied to every repo at once (better than email-invite reclaim).
- Smart HTTP clone and push, with push to a protected branch going through Land.
- Three reversible phases: mirror, bridge, cutover.
- A fidelity report for every import; never claim "100%" without it.
