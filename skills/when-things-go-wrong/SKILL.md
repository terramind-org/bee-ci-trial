---
name: when-things-go-wrong
description: Use this to recover from conflicts, failed checks, a stopped run, a safety tripwire, a spent budget or an expired lease, or to raise a concern and stop cleanly.
---

# When things go wrong

1. **Save the facts.** Stop the affected action and record the repository,
   branch, run or pull request ID, the last step that worked, and the reason and
   subcode. Preserve authorized work and keep secrets out of the report. For
   a refusal, follow [Rules and refusals](../rules-and-refusals/SKILL.md) before
   retrying anything.

2. **Resolve a conflict in the working copy.** Run status in the workspace.

   ```sh
   bee status --json
   ```

   Status lists pending edits, conflicts and whether a rebase is in progress.
   If the target moved and the working copy is clean with no rebase underway,
   sync against the target.

   ```sh
   bee sync --json
   ```

   Sync reports the new head and remaining stack, or stops at conflicting
   files with exit 3. If there are uncommitted changes, preserve and inspect
   them before syncing. Where a rebase stopped, read both sides and their base.
   Edit the conflicting files to keep the intended behavior and remove the
   markers.

   ```sh
   bee resolve --json
   ```

   Resolve continues the rebase once all conflicts are resolved. If it stops
   again, inspect the next files. If you cannot resolve the conflict within
   scope, use the following to return to the pre-rebase state. Save any
   authorized resolution work you need to retain first.

   ```sh
   bee sync --abort --json
   ```

   This reports the restored head and stack. After any rebase, follow
   [Working alone](../working-alone/SKILL.md) to confirm the pull request head and
   rerun checks for that revision before attempting a merge.

3. **Read failed checks and stopped runs.** Use the current pull request
   revision's checks to find the first failing check and its output. Fix what
   is in scope and check the new revision. For an infrastructure failure use
   the authorized rerun flow and report the original failure. Preserve required
   checks and their assertions. To inspect a run, use `<run-id>` from its Agents
   page or `BEE_RUN_ID` in a managed run.

   ```sh
   bee agent get-agent-run --id "<run-id>" --json
   bee agent list-agent-run-events --run-id "<run-id>" --json
   ```

   The first response gives the run state, summary and any failure reason.
   The second lists recorded events. If you cannot read them, send the IDs and
   visible error to the person responsible, or to the Queen · lead in a Swarm.
   A cancelled standalone run is terminal. To stop a live run deliberately,
   the person with permission can use Stop on its run page. Once the cause is
   cleared, an authorized person can retry it from that page. A retry creates
   a new run. A regular Swarm pause holds new starts and lets in-flight workers
   hand back.

4. **Honor safety stops.** A tripwire creates a Needs you entry for the
   accountable person. It can cancel a standalone run or pause a Swarm and
   cancel its active runs. Read the finding and the observed evidence. Wait for
   the person's decision. If an organization owner stops agent work, agent
   merges are frozen. The `safety_merge_frozen` subcode identifies that refusal.
   Only the organization owner can lift the freeze. A resume rechecks held
   merges. Paused Swarms and cancelled runs still need explicit actions to
   resume or retry.

5. **Stop spending and recover a lease correctly.** On `BUDGET_EXHAUSTED` or a
   spending quota error, stop work charged to that budget. Report what is done,
   what remains and the limit that was reached. Ask the authorized person for
   a decision, through the lead if assigned to a Swarm.

   To renew an active lease, use `<repo>` from the assignment and `<lease-id>`
   from the original acquisition.

   ```sh
   bee lease renew-lease --repo "<repo>" --id "<lease-id>" --ttl-seconds 3600 --json
   ```

   This returns the lease with its new expiry. If you confirm a `NOT_FOUND`
   means the lease expired, leave the files alone until you acquire a new one
   on the same approved paths using [Working alone](../working-alone/SKILL.md).
   If another holder now has it, coordinate before editing or merging.

6. **Raise a concern if the task conflicts with rules, looks unsafe or may be
   unlawful.** Use the credential issued for your own active run. Write
   `<concern-text>` as a short description of the instruction, why you stopped
   and the supporting evidence. Choose `rule-conflict`, `unsafe` or
   `potentially-unlawful` as `<category>`.

   ```sh
   bee safety raise-concern --repo "<repo>" --run-id "<run-id>" --category "<category>" --text "<concern-text>" --json
   ```

   The operation records the concern, stops the run and creates a separate
   Needs you entry for the accountable person. This pauses the task's progress
   even though the current run is cancelled. A concern never counts against the
   agent or contributes to a safety verdict. Since cancellation can end the
   caller's process, put the essential evidence in the request. If submission
   fails or confirmation is lost, stop the risky action and report what you
   know through your authorized contact.

7. **Stop cleanly.** Leave a short record of the completed work, checks,
   any unmerged pull request, the exact blocker and who can make the next
   decision. Release any active lease you no longer need if you still hold
   the credential. If you are in a Swarm, hand back the attempt while it is
   active and send routine updates through the lead. Say which step must
   happen before work can resume.
