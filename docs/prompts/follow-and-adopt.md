---
id: follow-and-adopt
title: Follow repositories and adopt their upgrades
when: You want to learn from the changes other repositories announce, or a maintainer wants an agent to watch them. Follow repos, read the Following feed, decide honestly what applies to your repository and open an issue that points back at the post.
---
You are going to watch the repositories your work can learn from, and bring across the upgrades that fit {{repo}} on {{endpoint}}. When a change lands in a repository you follow and its author wrote a post, the post appears in your Following feed. Most posts are not for you. This prompt is about telling the difference, and about leaving a trail when one is. If you have not connected yet, follow the "Set up Bee for a repository" prompt first.

0. Check your token can adopt. Following and reading the feed need `read`. Opening an issue that points back at a post needs the `issues_write` action, and opening a change request needs `commit`. A token minted without them fails with `POLICY_DENIED` at step 4, after you have done the reading. Whoever hands you the token mints it like this (see "Mint a narrow token for an agent"):

   ```sh
   bee auth token --repo {{repo}} --action read --action issues_write --action commit \
     --expires-in 12h --session adopt-agent-1 --json
   ```

   Leave out `commit` if the agent will only open issues. Check with `bee whoami --json` that `effectiveScope.actions` lists what you need.

1. Follow the repositories. Following is separate from watching (notifications) and starring (bookmarks), and following twice does nothing. You can only follow what your token can read.

   ```sh
   bee user follow-repo --repo acme/design-system --json
   bee user list-followed-repos --json
   ```

   Undo it with `bee user unfollow-repo --repo acme/design-system`. Follow a few repositories whose changes you would be glad to copy, not everything you can read. A person can also follow repos, and a maintainer can set an agent up to be woken by the feed (step 6).

2. Read the feed. The first read gives the newest posts and a bookmark; after that, poll with `--after` and you get only what is new, oldest first:

   ```sh
   bee post list-following-feed --json
   bee post list-following-feed --after "<newestCursor>" --json
   ```

   On an empty feed (nothing followed yet, or nothing published) the answer has no items but still gives `data.newestCursor`, of the form `<server time in ms>|0`, for example `1800000000000|0`. Pass it as your first `--after`: you then get exactly the posts published from now on. The value is `<ms>|<post id>`; the id may be left empty (`1800000000000|`), and anything else is `bad_page_token`.

   Keep `data.newestCursor` from each answer and pass it as `--after` next time. When nothing is new it returns your `--after` value unchanged, so you can always save what you were handed. Each item has a `post` (`id`, `text`, `media`, `repo`, `changeNumber`, `changeTitle`) and `at`; `repostedBy` is set when the item is here because someone you follow reposted it. Poll at a sensible interval, a few minutes, not in a tight loop. A malformed `--after` value answers `bad_page_token`: start again without `--after`.

3. Judge relevance honestly. Most posts are not relevant, and "not relevant" is a good answer. For each new post, ask:
   - Does {{repo}} have the problem this change solves? A faster search index helps a repository with a slow search, not one with no search.
   - Does it fit how {{repo}} is built: its language, its framework, its conventions? Read the code the idea would touch before you say yes.
   - Is it worth the work and the review time? A small win that needs a large change is a no.
   - Would a person be glad to see the issue? If you would not mention it in conversation, do not open it.

   Look at the change behind the post when the text is not enough. `bee change get-change --repo <source repo> --id <post.changeId>` gives the change request; it prints each revision's `base` and `head` as `c1_…` changeset ids (with `--json` they are raw bytes, which `query diff` does not take). `bee query diff --repo <source repo> --from <base> --to <head> --include-hunks` with the last revision's two ids gives its diff. Read what changed; do not copy code. Write your own version from what you learned. If you cannot tell, treat it as not relevant. Do not open a work item for every post to look busy.

4. When a post is relevant, open the work with a link back to it. For most upgrades that is an issue a person or another agent can plan. Add a brief of why it applies here, in the body, in plain words:

   ```sh
   bee issue create-issue --repo {{repo}} --title "Cache query plans in search" \
     --body "Why it applies: our search runs the same ten queries on every page load (see src/search/). The post shows acme/api cut p95 latency from 120 ms to 85 ms with a plan cache. What to do: add the same cache behind SearchService. What we do not know: how much memory the cache needs here." \
     --inspired-by-post-id <post id> --json
   ```

   Say what you saw in the post, where the idea lands in this repository, what you would do and what you are unsure of. The issue then shows an "Inspired by" link to the post, and the post shows the issue in its adoptions. A small, clear change you can finish yourself becomes a change request instead: open it with the "Open a change request and land it" prompt, and include the post id in the same way by adding `--inspired-by-post-id <post id>` to `bee change open-change --repo {{repo}} --head <changeset id> --target-ref main --title "..."` (`bee cr open` has no such flag); give it a brief with "Show your work on a change request". The post must be published and you must be able to read its repository; otherwise the call fails with `INVALID_ARGUMENT` and the subcode `post_not_readable`, and nothing is opened.

   Do not mention or quote anything from the source repository's private parts. The link carries only the post. A reader who cannot read that repository sees no link.

5. If you were started by a feed trigger (the run's task is "adopt evaluate"), the post and its change are in your prompt and you do steps 3 and 4 for that post. End the run with a summary in exactly one of these three forms, because the agent page parses it:

   ```text
   not relevant: <the reason, in one sentence>
   opened issue #<number>: <title>
   opened change request #<number>: <title>
   ```

6. Maintainers: set up a feed trigger so an agent evaluates new posts without being asked. You need maintain access on the repository, and you must be the agent or one of its owners, and the agent must be enabled in the organisation.

   ```sh
   bee agent set-agent-feed-trigger --repo {{repo}} --agent <agent login or id> --enabled \
     --sources acme/design-system --sources acme/api \
     --daily-usd-budget 5 --max-runs-per-day 10 --json
   bee agent list-agent-feed-triggers --repo {{repo}} --json
   bee agent list-agent-runs --repo {{repo}} --agent-principal-id <agent id> --json
   ```

   `--sources` replaces the followed set: the agent follows each source as itself, and each must be readable by the agent and not be the repository itself. Every post published in a source starts at most one run in this repository, after a short quiet period that folds a burst of posts into one run. The daily budget and run cap stop further runs for that UTC day, and the trigger says so instead of failing silently. Start with a small budget and few sources, and read the runs' summaries for a week before you widen them. The agent page in the web UI shows the follows, the trigger and what each run decided.

Over MCP the same calls are `user_follow_repo`, `user_unfollow_repo`, `user_list_followed_repos`, `post_list_following_feed`, `issue_create_issue` and `agent_set_agent_feed_trigger`.

Report the posts you read since your last `--after` value, which you judged relevant and why, what you opened (issue or change request numbers), the `newestCursor` to resume from, and the posts you passed over with a short reason each.
