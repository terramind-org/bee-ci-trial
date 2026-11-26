# Forge change requests (`bee-forge::changes`)

Change requests, revisions, reviews, threads, auto-merge, the merge queue and the repo
context API. Contract: SPEC §7.4, §9.4, §11.2–§11.4, §11.6; PRODUCT §4.13. Conventions:
`docs/spec/forge-core.md` (one transaction per store method, `emit_event` + `audit`,
`ForgeError`, the service shape).

## 1. Layout

| path | owns |
|---|---|
| `changes/mod.rs` | `MODULE`: surface `changes`, tables, migration `changes/0001_create_changes`, event kinds, audit prefixes |
| `changes/model.rs` | private row records (`CrRecord`, `CommentRecord`, `ViewedRecord`) |
| `changes/vcs.rs` | `ChangeVcs` and `Lander` (what CRs need from the VCS), `HistoryVcs` over `bee-history`, `NoLander` |
| `changes/brief.rs` | the brief (SPEC §11.9): pure validation, `Changes::set_brief`, `NewChange.brief` (`OpenChange.brief`), `Changes::revision_paths` for the checklist; see `docs/spec/forge-briefs.md` |
| `changes/text.rs` | closing keywords, anchor carry-forward, suggestion application (pure) |
| `changes/store.rs` | `Changes`: lifecycle, metadata, stacks, templates, conversation and changeset comments |
| `changes/review.rs` | reviews, review requests and auto-assignment, threads, suggestions, viewed files, revision diffs |
| `changes/landing.rs` | land decisions, landing, auto-merge, merge queue, land conflicts, revert, update base, repo context |
| `changes/query.rs` | `ListChanges` filters and the query language |
| `changes/service.rs` | `ChangeServices`: the tonic `ChangeService` and `RepoService.GetContext` |

## 2. Model

- **CR** row: scope repo, id = zero-padded number, `k1` = CR id (unique), `k2` = current head
  (hex, for check events), body = `ChangeRequest` plus private state: changed paths with
  their class and size, stack members, pusher per revision, keyword and manual issue links,
  reactions, comment count, pre-land tip, queue method, land-intent chain.
- **Numbers** come from a per-repo counter (`forge_cr_counters`).
- **Stack**: the first-parent chain from the head back to the first ancestor of the target
  tip, or of the parent CR's head when `depends_on` names an open CR. `change_ids` are the
  members' ChangeIds, so a stacked CR covers only its own changes.
- **Revision** on every new head or new base: `{n, head, base, diff_class, created_at}`;
  `diff_class` is the highest class over the members' `diff_class` (bee-derived, policy
  `generated`/`sensitive` globs, fingerprint = policy epoch). Revising to the same head and
  base is a no-op. A new revision drops the CR from the merge queue.
- **Brief** (`ChangeRequest.brief`, SPEC §11.9): stored on the CR and written with its
  version, by `Changes::set_brief` (`BriefService.SetChangeBrief`) or with the CR
  (`OpenChange.brief`, validated before anything opens). The server sets
  `written_for_revision`, `author` and `updated_at`. Evidence and the checklist are the briefs
  module's (`docs/spec/forge-briefs.md`).
- **Edit** of target or parent records a new revision; landing a parent retargets its open
  children onto the ref (`depends_on` cleared, `cr.edited`).

## 3. Reviews and threads

- Verdicts approve, request changes, comment. Authors may only comment on their own CR. A
  review answers the reviewer's pending request. `stale` = made on an older revision;
  `counts_toward_policy` = approval, not dismissed, not the author, current revision when
  the land rule dismisses stale approvals, human unless an approval rule includes agents.
- With the land rule's `dismiss_stale_approvals`, pushing a revision dismisses older
  approvals (`review.dismissed`); without it they are only marked stale.
- Threads anchor to `(path, side, start..=end)` on a revision (side `new` = head, `old` =
  base; a file-level thread has no lines). On each new revision a live thread is compared
  between its revision and the new one with `diff3-v1` line hunks: unchanged lines move
  with the edits above them and the thread advances; otherwise it is marked outdated
  (`review_thread.outdated`) and keeps its revision. Comments always show their thread's
  anchor.
- Suggestions (side `new`, lines only) apply singly or in batches: non-overlapping edits per
  file on the current head become one child changeset authored by the applier with the
  applier's delegation chain as provenance, recorded as a new revision; the suggestions are
  marked `applied_in`. Outdated suggestions refuse to apply.
- "Changes since my review": `from = 0` is the caller's last reviewed revision. Equal bases
  diff the heads; different bases compare only paths either revision changes, so upstream
  changes from a rebase do not show.
- Viewed files store the file's blob at the viewed revision; `changed_since` compares it
  with the current head.

## 4. Review requests

Opening a CR (or marking a draft ready) requests the owners of its paths (`[[owners]]`, else
`CODEOWNERS`): principal owners directly, `@group` owners as team requests, `@login` owners
through the directory. Team requests expand by the team's `TeamReviewAssignment`
(`TeamDirectory`, owned by enterprise): round robin over a per-team counter, or load balance
by outstanding requests in the repo; the author, excluded members and (with `skip_agents`)
agents are skipped; the team request stays only with `notify_team`.

## 5. Landing

The server owns landing (SPEC §7.4); this module decides and records.

- **Decision** (`EvaluateLanding`): `Policy::evaluate_land` (or a default allow-writers
  policy when the repo has none) over the current revision: changed paths and classes, the
  chain of the caller, reviews (non-dismissed, revision = reviewed head), the latest check
  per name of the head (`Checks::check_results`), live leases of the target ref
  (`Leases::policy_leases`, fail-open on outages), stack members, merges, last pusher.
  `missing` adds `ready_for_review` for drafts, `parent:#N` for an open parent and
  `state:open`. `mergeable` = the stack rebases cleanly onto the tip.
- **Land** (`LandChange`): open, not a draft (`DRAFT`), parent landed, expected revision,
  method allowed by repo settings (`land_method_not_allowed`), not when the target ref requires
  the merge queue (`merge_queue_required`: the repo setting, or an active `merge_queue` ruleset
  matching the ref, answered by `LandFacts::merge_queue_required`), policy allowed
  (`POLICY_DENIED` with missing ids, or `LEASED` with the conflicting leases). Then `Lander::land`; `MERGE_CONFLICT` records a land
  conflict. On success `record_landed`: state, `landed_ref`, pre-land tip, queue log,
  conflicts resolved, children retargeted, `cr.landed` (body = `ChangeRequest` with
  `change_ids`; `payload_json` = `closes_issues`, `retargeted`, landed ref and version).
- **Merge methods**: rebase (default), squash (one changeset, default message
  `title (#n)\n\nbody`), merge commit (parents tip and head, `Land #n: title`).
- **Auto-merge**: stores method, message and the enabling chain; tries at once, then
  `AutoMergeTrigger` (an `EventListener`) retries on `check.updated` for the CR's head,
  `review.submitted`/`review.dismissed`, `cr.ready_for_review`, `cr.revised`, and for
  children retargeted by a parent's `cr.landed`. When the target ref requires the merge queue
  (setting or ruleset) it enqueues.
- **Merge queue**: entries per ref ordered by priority then arrival; enqueue needs every
  requirement but checks, a stacked CR needs its parent queued or landed, priority > 0 needs
  maintain. `GetMergeQueue` returns positions, estimated land times and lands per minute
  (last 10 minutes). The coalescer reports states (`set_queue_state`; `EJECTED` removes) and
  lands (`land_queued`, or its own land + `record_landed(via_queue)`); `process_queue` is a
  reference one-step coalescer. An active matching API `merge_queue` ruleset can set
  `max_entries_to_merge`: it is an enforced per-ref queue capacity, so an enqueue beyond it
  returns `RESOURCE_EXHAUSTED` / `merge_queue_full`. If several matching active rulesets set a
  capacity, the smallest non-zero maximum wins. The queue's one-at-a-time behavior fixes
  `min_entries_to_merge` at one (zero means omitted), `check_timeout_seconds` at zero, and
  `all_entries_must_pass` at false (entries are independent). `merge_method`, `max_entries_to_build`, and
  `min_entries_wait_seconds` are refused as `INVALID_ARGUMENT` / `unsupported_rule`; so are a
  non-zero timeout, a batch minimum above one, and `all_entries_must_pass: true`.
  The server's queue worker runs `step_queue`: by default the
  head entry waits while only checks (of its own head) are missing and is landed otherwise.
  With the repo setting `merge_queue_check_combined` the queue tests the combined result: the
  head entry's stack is rebased onto the target tip (conflict: eject `MERGE_CONFLICT`), the
  entry goes `CHECKING` with that changeset as `speculative_head` (its base is kept in the CR
  row), `merge_queue.checks_requested` (GitHub `merge_group`/`checks_requested`; payload
  `base_ref`, `base_sha`, `head_sha`, `head_ref`, `head_commit_message`) starts the
  `merge_group` workflows on it, and required checks are read from it: a failure ejects the
  entry (`POLICY_DENIED`, comment naming the checks), pending checks wait, success lands
  exactly that changeset (land inputs read its checks first). A result built on an older tip
  is rebuilt; requirements other than checks are evaluated as for enqueueing. Entries are
  tested one at a time in queue order.
- **Landed index**: `record_landed` writes `forge_cr_landed` (changes migration 3): every
  changeset the land put on the ref (the CR's own landed members, and the squash or merge
  result) → the CR. `Changes::landed_by` reads it (post-land bisect).
- **Conflicts**: held per CR; a submitted resolution becomes the CR's new revision and,
  with `land`, enables auto-merge; the land marks the conflict resolved.
- **Revert**: restores the pre-land tip's content of the paths the land changed, rebased onto
  the current tip, as a new CR (`revert_of`, title `Revert "<title>"`).
- **Update base**: rebase the stack onto the tip (or parent head), or merge the tip in.

## 6. Other surfaces

- Linked issues: manual links plus closing keywords (`close[sd]`, `fix(es|ed)`,
  `resolve[sd]` + `#n` or `org/repo#n` of this repo) in the body and the stack's messages.
  Keyword links and, with `close_on_land`, manual links close on land via `cr.landed`.
- Labels (names), assignees, milestone (id) are references; the issues module owns them.
- Templates from the target tip: `.github/pull_request_template.md` (and the usual
  alternatives) as `default`, plus `*.md` in `.github/PULL_REQUEST_TEMPLATE/`.
- Conversation comments, reactions (CR and comments), locking (only triagers comment on a
  locked CR, `LOCKED` otherwise), summaries with their author, changeset comments.
- Repo context (`GetContext`): owners per path, live leases, open CRs touching the paths,
  failing checks at `at`, land-rule summary of the default ref, `AGENTS.md`/`CLAUDE.md` on
  the way to each path and `.github/agents/*.md` (64 KiB each), policy epoch, sensitive paths.
- `ListChanges`: request filters plus the GitHub-compatible query (`bee_search`
  `ForgeKind::Issues`): state, type, draft, locked, queued, author/assignee/commenter/
  involves/mentions/reviewed-by/review-requested (logins resolved by the directory, `@me`),
  agent, label, milestone, base, head, repo/org, team-review-requested, path, review
  decision, combined check status, linked, created/updated/closed/merged, comments,
  reactions, interactions, risk, `no:`, free text in title/body, sort. Offset page tokens.

## 7. Events and audit

Events: `cr.opened/revised/landed/closed/reopened/edited/ready_for_review/converted_to_draft/
review_requested/review_request_removed/labels_changed/assignees_changed/milestone_changed/
issues_linked/summary_set/brief_set/auto_merge_enabled/auto_merge_disabled/locked/unlocked/reacted`,
`cr_comment.created`, `review.submitted/dismissed`, `review_comment.created`,
`review_thread.created/resolved/unresolved/outdated`, `changeset_comment.created`,
`merge_queue.entered/updated/left/checks_requested`, `land_conflict.opened/resolved`. CR events carry the
`ChangeRequest` body, review events the `Review`; thread and comment events carry the subject
as proto3-JSON. Every state change writes an audit entry under `cr.`, `review.`,
`merge_queue.`, `land_conflict.` or `changeset_comment.`.

## 8. Authorization

| RPCs | action |
|---|---|
| reads, conversation and changeset comments, viewed files | `read` |
| open, revise, edit, ready/draft, summary, apply suggestions, update base, revert, resolutions | `commit` |
| brief (`BriefService.SetChangeBrief`) | the CR's author (with `read`) or `commit` |
| close, reopen, labels, assignees, milestone, issues, review requests | `triage` |
| submit review, line comments, replies, resolve/unresolve | `review` |
| dismiss a review | `manage_settings`, maintain |
| land, auto-merge, enqueue, dequeue | `land` on the CR's paths and target ref |
| queue priority > 0 | also `manage_settings`, maintain |

## 9. Server wiring

1. `Changes::new(&forge, ChangeHooks { vcs, lander, teams })` with a VCS-backed `ChangeVcs`
   (`HistoryVcs` over the server's `Writer`), the coalescer's `Lander`, and the enterprise
   `TeamDirectory`; then `changes.attach(&forge)` (auto-merge trigger).
2. `ChangeServices::new(forge, changes).register(&mut routes)`; `RepoService.GetContext`
   delegates to `ChangeServices::get_context`.
3. Lands that do not go through `LandChange` (a push landing a CR, the coalescer's merge
   queue) call `record_landed`; conflicts call `record_conflict`; queue progress calls
   `set_queue_state`; a moved CR head calls `revise`.

## 10. API gaps

- CR numbers come from the changes surface; issues need the same allocator so numbers stay
  shared (SPEC §11.8). Proposed: one per-repo number counter both modules call.
- Closed (2026-10-21): signature states, enforced required workflows and open code-scanning
  alerts reach `Changes::decide` through the `LandFacts` port the server installs
  (`Changes::set_land_facts`); without it changesets count as unsigned.
- `TeamReviewAssignment.count_child_team_members` is ignored (no team hierarchy in
  `TeamDirectory`).
- The policy action set has no merge-queue jump permission; priority needs maintain.
- `ContentService.AddReaction` does not delegate CR subjects; CR reactions use
  `ChangeService.AddChangeReaction` / `RemoveChangeReaction`.

Closed (2026-10-19): reaction and lock RPCs (`AddChangeReaction`, `RemoveChangeReaction`,
`SetChangeLocked`), `Comment.anchor` (line and changeset comments, with the thread id),
`ChangeRequest.closing_issues`, `ChangeRevision.pusher`, `MergeQueueEntry.land_method` and
`on_behalf_of` (also on `AutoMerge`), `LandingEvaluation.rule` / `reason`, and
`DiffChangeRevisionsRequest.paths` / `ignore_whitespace`.
