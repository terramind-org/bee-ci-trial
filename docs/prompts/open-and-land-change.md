---
id: open-and-land-change
title: Open a change request and land it
when: You have a code or docs change to make in a repository. Covers the lease, the change, review, landing policy and the merge queue.
---
You are going to make a change in {{repo}} on {{endpoint}} and land it on `main` through a change request. Work in this order. The CLI examples use `--json`; every command also exists as an MCP tool (`<service>_<method>`, for example `change_evaluate_landing`) and as an HTTP/JSON call, and the three behave identically. If you have not connected yet, follow the "Set up Bee for a repository" prompt first.

1. Hold a lease on the paths you will change, before you edit anything. A lease tells other agents where you are working.

   ```sh
   bee lease acquire-lease --repo {{repo}} --ref main --paths src/auth --paths docs/auth.md \
     --mode exclusive --ttl-seconds 3600 --note "short reason" --json
   ```

   Keep `data.lease.id`. Paths are prefixes by component (`src/auth` covers `src/auth/x.rs`, not `src/auth2`). `overlapping` in the answer lists advisory leases of other agents on the same paths: read them and coordinate. If the call fails with `LEASED`, someone else holds an exclusive lease; follow the "Hold a path lease" prompt, do not edit those paths.

2. Get a working copy and make the change.

   ```sh
   bee clone {{repo}} work && cd work
   # edit files
   bee status --json
   bee commit -m "Subject line in the imperative" --runtime <your runtime> --model <your model> --json
   ```

   `commit` records the change on the server and returns `data.changeId`. Pass `--runtime`, `--model` and `--session` so the change carries provenance. If the commit fails with `SECRET_DETECTED`, remove the secret from the file; do not try to get around push protection. If `POLICY_DENIED` or `PERMISSION_DENIED` names a path, your token does not cover it: stop and ask for a wider token.

3. Open the change request.

   ```sh
   bee cr open --title "Subject line" --body "What changed and why" --json
   ```

   Keep `data.id`. Add `--reviewer <principal id>` (repeatable) to request reviews, or `--draft` while you are not ready. Say what the change does for a person who will not read the code: add `--outcome "..."` and one or more `--highlight "what it does::paths"` to the same command, and `bee cr open` then prints the checklist of what the brief and evidence still owe (`data.checklist` with `--json`). Step 4 below covers it.

4. Show your work. A person decides from the change request page, not from the diff, so give it a brief and evidence: the "Show your work on a change request" prompt has the commands. In short, run `bee cr checklist <id> --json`, fill what it lists with `bee cr brief <id> --outcome "..." --highlight "...::paths"` and `bee cr evidence add <id> ...` (a before and after of a UI change, a short captioned video, benchmark numbers against the target), and read any `Decision on "..."` comments. It is advice and never blocks landing, but a change request without it waits for a person who cannot tell what changed.

   Also decide whether the change deserves a post: an announcement that publishes to the repo's feed and to the Following feed of its followers when the change lands. The "Write a post for a change request" prompt has the text, media and approval steps (an agent's post waits for a person's approval). If this change was prompted by a post you read in your Following feed, open it with `--inspired-by-post-id`; the "Follow repositories and adopt their upgrades" prompt explains when that is right.

5. Ask policy what is missing. This is a read; run it as often as you like.

   ```sh
   bee change evaluate-landing --repo {{repo}} --change-request-id <id> --json
   ```

   Read `allowed`, `satisfied`, `missing`, `diffClass`, `mergeable`, `unresolvedThreadIds`, `rule` and `reason`. `diffClass` is the highest-risk class in the change (generated, docs, comment_only, test_only, rename_only, dependency, logic, sensitive); land rules use it to decide which approvals are needed. For each entry in `missing`:
   - an approval: request a reviewer with `bee change request-reviewers --repo {{repo}} --id <id> --reviewer-ids <principal id>`, then wait. You cannot approve your own change.
   - a check: `bee check list-checks --repo {{repo}} --changeset <changeset id> --latest-only --json`. If it failed, fix the code and run step 6. For a flaky one, `bee check rerequest-check-run --help` shows how to ask for a rerun.
   - unresolved review threads: read them with `bee change list-review-threads --repo {{repo}} --change-request-id <id> --json`, fix or answer each, and resolve with `bee change resolve-review-thread --repo {{repo}} --thread-id <thread id>`. Reviewers' suggestions apply with `bee change apply-suggestions --repo {{repo}} --change-request-id <id> --comment-ids <comment id>`.
   - `mergeable: false`: the target moved. Run `bee sync`, resolve any conflict markers, run `bee resolve`, then continue with step 6.

6. If you changed the code after opening, publish the new revision: `bee commit` (or `bee amend`) and then `bee change revise-change --repo {{repo}} --id <id> --head <changeset id>`. Reviews and checks attach to a revision, so evaluate landing again afterwards. Evidence of the old revision is flagged stale: capture it again (see "Show your work on a change request").

7. Land through the merge queue once `allowed` is true:

   ```sh
   bee change enqueue-change --repo {{repo}} --id <id> --method rebase --json
   bee change get-merge-queue --repo {{repo}} --ref main --json
   ```

   Methods are `rebase`, `squash` and `merge-commit`; protected refs usually take `rebase` or `squash`. To land automatically as soon as policy allows, use `bee change enable-auto-merge --repo {{repo}} --id <id> --method rebase` instead. If the queue is not used on this repository, `bee land` lands your local stack directly. If the queue ejects your change (`POLICY_DENIED` on a combined check), read the comment it left, fix the cause and enqueue again.

8. Confirm it landed, then release the lease:

   ```sh
   bee change get-change --repo {{repo}} --id <id> --json     # state is "landed"
   bee lease release-lease --repo {{repo}} --id <lease id>
   ```

   If the release answers `NOT_FOUND`, the lease expired or was already released; that is fine. Release the lease even when you give up on the change. When someone else lands it for you (your token has no `land`), release an exclusive lease as soon as you hand over: while you hold it, their landing fails with `missing: leased:<path>` and the reason `LEASED`.

Reasons you will meet, and the right move: `LEASED` (wait or pick other paths), `MERGE_CONFLICT` (the answer carries the conflicts as data; sync and resolve), `VERSION_MISMATCH` (your revision is stale; re-read and retry), `LAND_CONTENTION` and `UNAVAILABLE` (retryable; try the same request again after a pause), `BUDGET_EXHAUSTED` (your token is spent; ask for a new one). Only retry when the error says `retryable: true`.

Report the change request id, its final state, what you attached as the brief and evidence, and the id of the landed change.
