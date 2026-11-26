---
name: leading-a-swarm
description: Use this when you are the Queen · lead coordinating tasks, join requests, decisions and outcomes for a Beehive Swarm.
---

# Leading a Swarm

1. **Confirm the goal and the branch.** Ask the accountable person for the goal,
   acceptance checks, limits and branch. The person who owns the lead run creates
   the Swarm. For that call use `<repo>` from the run, `<branch>` from its
   `baseRef`, `<lead-run-id>` from the run record, and the approved `<name>`
   and `<goal>`.

   ```sh
   bee swarm create-swarm --repo "<repo>" --name "<name>" --goal "<goal>" --branch "<branch>" --lead-run-id "<lead-run-id>" --json
   ```

   This returns the new Swarm in planning. Keep its `data.id` as `<swarm-id>`.
   If creation fails, check that the lead run is active on that branch, has the
   right owner and has no existing membership. Use the lead run's credential for
   its work below.

2. **Give each worker an outcome and separate paths.** For each task, write
   `<task-title>` as an action, and supply each repository-relative `<path>`
   with a separate `--paths` flag. Keep paths disjoint, including parent and
   child directories. Put shared changes in their own task and wait for it before
   starting dependent work. Give each worker the acceptance checks and where to
   reach you.

   ```sh
   bee swarm add-swarm-task --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --title "<task-title>" --paths "<path>" --json
   ```

   The response returns the open task ID and its paths. Share that ID with the
   worker. If paths are invalid, use repository-relative paths without dot
   components or trailing slashes. If an attempt cannot claim paths, inspect the
   active work and schedule or split the task before another attempt.

3. **Keep one current line and answer workers.** Read the Swarm and hand-backs.
   Use `<status>` for one line naming the current result and the next blocker or
   decision. Update it when that changes.

   ```sh
   bee swarm get-swarm --repo "<repo>" --id "<swarm-id>" --json
   bee swarm set-swarm-status --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --status "<status>" --json
   ```

   Both calls return the Swarm, including the visible tasks and status. If setting
   status fails, confirm you are using the lead run credential and the Swarm is
   active. Answer workers through their assignment channel. Bring decisions that
   need the person to that person yourself. Include the choice, affected work and
   what continues meanwhile. A person's steering in Beehive goes to the lead run.

4. **Decide pending joins.** Read the applicant, agent and budget from
   `data.joins`. Use the pending entry's `id` for `<join-id>`. Check whether the
   repository's rules and the work allow this person and agent to join.
   Choose `true` to accept or `false` to decline as `<accept>`. Give a short
   `<reason>`.

   ```sh
   bee swarm decide-swarm-join --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --join-id "<join-id>" --accept="<accept>" --reason "<reason>" --json
   ```

   This returns the decision and, when accepted, the queued run ID. Policy can
   return a persisted refusal. Keep the recorded reason and explain it to the
   applicant. If the join changed, read its current decision before responding.

5. **Coordinate starts, pauses, stops and finish.** State changes require the
   accountable person or a repository admin with the relevant access. Use
   `working` for `<state>` to start or resume, `paused` to hold starts,
   `stopped` to end the Swarm and cancel live runs, or `finished` after all
   work ends.

   ```sh
   bee swarm set-swarm-state --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --state "<state>" --json
   ```

   This prints the updated Swarm. A regular pause holds queued members and new
   attempts while in-flight workers can hand back. Treat stop as terminal.
   A finish needs every task ended and every member run terminal. If the state
   change is refused, read the current state and resolve its prerequisite with
   the person. An organization stop must be cleared before resuming.

6. **Close the loop.** For a task that came back empty, read the attempt's reason
   and hand-back. Decide whether to narrow it, clear a dependency, or record an
   unmet outcome. Before finishing, read the Swarm again and collect its
   `changeRequestIds`. They are attributed links from member runs. Confirm each
   pull request's state and send the person its number or link, the checks,
   remaining decisions and any unmet goal. Keep empty attempts visible alongside
   the work that shipped.
