# Where GitHub breaks, and why (July 2025 – October 2026)

Research note behind SPEC §15. Compiled 2026-10-17 from GitHub's monthly availability reports
(July 2025 – September 2026) and the githubstatus.com feed for October 2026. The figures were
gathered through a summarizing fetch, so **check each one against its source before quoting it
publicly**. The June 2025 report was not read.

Sources: monthly reports at
`https://github.blog/news-insights/company-news/github-availability-report-<month>-<year>/`
(index: https://github.blog/tag/github-availability-report/), the CTO's post
https://github.blog/news-insights/company-news/an-update-on-github-availability/ (2026-04-28),
and https://www.githubstatus.com/history.rss.

## Notable incidents

| Date | Surfaces | Duration | Stated cause |
|---|---|---|---|
| 2025-08-05 | PRs, pushes, webhooks, REST | 32 min | Migration dropped a column the ORM still queried |
| 2025-08-12 | Search, and through it Issues/PRs/Projects/Actions | 3 h 44 min | LB→search connectivity failed; retry queues then overwhelmed the LBs |
| 2025-11-18 | All git operations | 1 h 04 min | Expired internal TLS certificate |
| 2026-01-15 | Issues, PRs, Actions, API, login | 1 h 40 min | Datastore major-version upgrade caused contention |
| 2026-02-09 | Web, API, Actions, git, webhooks | ~2 h 43 min | Settings-cache config change → mass rewrites → git HTTPS proxy connections exhausted |
| 2026-03-03 | Web ~40%, API ~43%, git HTTP ~6% failing | 1 h 10 min | Cache deploy bug rewrote every user's entry → replication lag |
| 2026-04-23 | Merge queue | — | Squash-merge regression reverted earlier changes in 2,092 PRs across 658 repos |
| 2026-04-27 | Search → Issues/PRs/Actions/Packages | 6 h 31 min | Anonymous scraping from 600k+ IPs |
| 2026-05-04 | PRs and nearly every surface incl. git, webhooks, OAuth | 55 min | Online schema migration on a hot table saturated a primary |
| 2026-05-06 | Creating PR review threads | 3 h 39 min | 32-bit Vitess lookup key hit its maximum |
| 2026-07-08 | Web, REST, GraphQL, Actions, git | 7 h 04 min | Automation changed runtime config on running VMs; service discovery broke; 5xx peaked at 96% |
| 2026-07-19 | github.com and data-residency environments | 2 h 11 min | DB fault fed incomplete data to the DNS control plane, pushed fleet-wide |
| 2026-07-24 | Creating PRs | 57 min | Cancelling a Vitess backfill dropped the target table |
| 2026-08-06 | All of Actions, Pages, Dependabot | 10 h 42 min | Deploy cut capacity near limits; sidecar throttling and OOM |
| 2026-08-17 | Issues, PRs, APIs, auth, webhooks, Actions | 7 h 35 min | LB flow exhaustion + mesh limits; a client retry bug amplified auth traffic |
| 2026-09-13 | Issues (96% of creates failing), PRs, tokens | ~2 h | Cleanup job overloaded a shared primary; safeguard only watched replica lag; no timeouts |
| 2026-10-07 | Git, PRs, Actions, Issues | ~10 min ×2 | DB-routing service started with stale records |
| 2026-10-12 | Issues, PRs, Projects, Actions, notifications | ~8 h 25 min | Expensive notification queries exhausted sharded replicas |

## Recurring causes (of ~70 incidents; bucketing is our judgement)

1. **Config, deploy and automation changes** (25+), often automation acting on incomplete input.
2. **Database:** migrations and ORM drift, shared-primary saturation, Vitess limits, routing (~15).
3. **Upstream and cloud providers** (~12, many more for Copilot's model providers).
4. **Capacity under spikes and abuse** (~9).
5. **Retry amplification** (a frequent second factor).
6. **Rate-limiter misfires** (5).
7. **Expired certificates and credentials** (5).
8. **Physical network faults** (4).

## How failures spread

- Faults in the core app and its shared MySQL have the widest blast radius: on 2026-05-04 one
  primary took down PRs, Issues, Actions, webhooks, git, OAuth and more.
- Git operations fail with forge metadata: settings caches, proxies, auth and DB routing all sit
  on the git path (2026-02-09, 03-03, 10-07, 2025-11-18).
- Search is a hidden dependency of issue and PR lists.
- Pages, Dependabot and Copilot agents run on Actions and fail with it.

## Agent load

- GitHub raised its capacity plan to 30× by February 2026, citing agentic workflows; merged PRs
  peaked at 90M a month (CTO post).
- On 2026-04-09 an agent-client update caused an API surge that hit a rate-limit bug; Copilot
  agent queue times reached 54 minutes.
- Limits that bind agents: 5,000 REST requests per hour per user (15,000 on Enterprise Cloud),
  100 concurrent requests, and 80 content-creating requests per minute / 500 per hour, shared
  per identity (https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api).
- GitHub added maintainer controls against PR floods, including a cap on open PRs per user
  without write access (https://github.blog/changelog/2026-06-17-limit-open-pull-requests-for-users-without-write-access/).

## What we took from it

Each SPEC §15 rule maps to a recurring cause: the git path doesn't depend on the forge; cells
with static stability; expand/contract schema changes; 64-bit keys; deadlines everywhere;
admission control on primary load; priority tiers and quotas per delegation root; a separate
anonymous pool; limiters that fail open; client retry budgets; last-known-good control planes;
automatic short-lived certificates; no third party on the critical path.
