---
id: review-change
title: Review a change request
when: You were asked to review a change request, or are on the reviewer list. Read the diff and its risk, comment on lines, suggest edits, and give a verdict.
---
You are going to review a change request in {{repo}} on {{endpoint}}. Review what the code does, not who wrote it, and give a verdict you can defend. If you have not connected yet, follow the "Set up Bee for a repository" prompt first. Your token needs the `review` action.

1. Find the change request and read it.

   ```sh
   bee cr list --repo {{repo}} --query "review:required" --json
   bee cr view <id> --repo {{repo}} --json
   ```

   Note the `revision` you are reviewing. Reviews and comments attach to a revision.

2. Read the brief before the code. The author should have said what the change does and shown it. Look at what they wrote and attached, and at what they still owe:

   ```sh
   bee cr evidence list <id> --repo {{repo}} --json
   bee cr checklist <id> --repo {{repo}} --json
   ```

   `data.brief` on `cr view` holds the outcome, highlights, risks, rollback and the decisions the author asked for. Evidence marked `stale` was captured for an earlier revision, and a change with an unmet item in `checklist` (for example `evidence:ui`) has not shown its work: say so in your review, and ask for it in a comment. Do not approve a UI change you have only read. See the "Show your work on a change request" prompt for what good looks like. A person answers the author's questions with comments that start `Decision on "..."`: if you are the person asked, answer in that form.

3. Read the risk before the lines. Ask what policy sees:

   ```sh
   bee change evaluate-landing --repo {{repo}} --change-request-id <id> --json
   ```

   `diffClass` is the highest-risk class in the revision, from lowest to highest: generated, docs, comment_only, test_only, rename_only, dependency, logic, sensitive. It sets how hard to look: a `docs` or `comment_only` change needs a read for correctness of wording; `dependency` needs you to check what was added and why; `logic` needs the behaviour traced; `sensitive` touches auth, payments, policy or CI configuration, so be strict and say so in your review. `missing` shows what the land rules still want, including how many approvals and whether an agent's approval counts.

4. Read the diff with per-path classes:

   ```sh
   bee query diff --repo {{repo}} --from <base> --to <head> --include-hunks --classify --json
   ```

   Take `<base>` and `<head>` from the revision in `cr view`. After the author pushes again, see only what changed since your last review with `bee change diff-change-revisions --repo {{repo}} --change-request-id <id> --from-revision <n> --to-revision <m> --include-hunks --json`. Read the files around the change, not only the hunks: `bee cat <path> --rev <head> --repo {{repo}}`.

5. Comment on lines. Anchor each comment to the new side of the file:

   ```sh
   bee change create-change-comment --repo {{repo}} --change-request-id <id> --revision <n> \
     --path src/auth/session.rs --side new --start-line 41 --end-line 44 --body "This returns before the lock is released."
   ```

   To propose the fix yourself, add `--suggestion '{"replacement": "<new text for those lines, without a trailing newline>"}'`. Use `--file-level` for a comment about the whole file. Say what is wrong, why it matters and what you would do. Mark nits as nits.

6. Apply suggestions that reviewers left on the change request (run it for the ones you accept): `bee change apply-suggestions --repo {{repo}} --change-request-id <id> --comment-ids <comment id> --message "Apply review suggestions"`. This creates a new revision, so read it again.

7. Give the verdict, with a body that states the reason in one or two sentences:

   ```sh
   bee change submit-review --repo {{repo}} --change-request-id <id> --revision <n> --verdict approve --body "Traced the new branch; tests cover both outcomes."
   ```

   `--verdict` is `approve`, `request-changes` or `comment`. Approve only a revision you read in full. Request changes when something must change before landing, and say exactly what. Use `comment` when you have questions and no verdict yet. A new revision from the author can make your approval stale; check `evaluate-landing` again.

Errors: `NOT_FOUND` for the change means it does not exist or you cannot see it; `DRAFT` means the author has not marked it ready; `PERMISSION_DENIED` means your token lacks `review`. Do not retry these. Report the change request id, the revision you reviewed, whether the brief and evidence showed the change, your verdict and the comments you left.
