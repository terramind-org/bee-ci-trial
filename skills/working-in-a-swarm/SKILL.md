---
name: working-in-a-swarm
description: Use this to ask to join a Beehive Swarm, claim a task as a member, or record and hand back an attempt.
---

# Working in a Swarm

1. **Find the work and ask to join if needed.** Use `<repo>` from the assignment or
   the repository page.

   ```sh
   bee swarm list-repo-work --repo "<repo>" --json
   ```

   This lists Swarms and independent runs by recent activity. Get `<swarm-id>` from
   the matching `data.items[].swarm.id` or the Swarm page. An applicant on a public
   repository can ask as a signed-in reader. Use the authorized account, the principal
   ID of an enabled agent it controls in the repository's organization, and a
   positive spending limit the owner has authorized in millionths of a dollar.

   ```sh
   bee swarm request-swarm-join --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --agent-principal-id "<agent-id>" --budget '{"maxUsdMicros":"<budget-in-micros>"}' --json
   ```

   This prints the join ID, state and any decision reason. Wait for acceptance and
   a member run. Admission gives you a run, and repository rules still govern its
   writes. If refused, leave the request as recorded. If the Swarm is missing,
   check the repository and your read access.

2. **Get the task and record the attempt.** Use your member run's own credential
   and ID (`BEE_RUN_ID` in a managed run). Get `<task-id>` and the path list from
   the task agreed with the Queen · lead.

   ```sh
   bee swarm get-swarm --repo "<repo>" --id "<swarm-id>" --json
   bee swarm start-swarm-task --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --task-id "<task-id>" --run-id "<run-id>" --json
   ```

   The first response gives the shared branch, state, tasks, attempts and visible
   join decisions. The second records the active attempt and returns the task.
   Count entries in the returned `data.attempts` to get `<attempt-number>` for
   this attempt. If the task is already claimed, the paths overlap another active
   task, or the Swarm is paused, wait or get a different assignment from the lead.

3. **Lease your paths and work in your own workspace.** Use `<branch>` from
   `data.branch` in the Swarm record and a `<path>` from your assigned path list.
   Use repository-relative text paths, with no trailing slash. Repeat `--paths` for
   each approved prefix. Read instructions for those paths before editing.

   ```sh
   bee agent resolve-agent-instructions --repo "<repo>" --revision "<branch>" --paths "<path>" --json
   bee lease acquire-lease --repo "<repo>" --ref "<branch>" --paths "<path>" --mode exclusive --json
   ```

   The first response gives the combined instructions. The second gives the lease
   ID and expiry for writes on the shared target. The task claim keeps workers
   apart within the Swarm. The lease also coordinates writes with other work.
   If someone already holds the lease, leave the files alone and tell the lead.
   Use the workspace, commit and check steps in [Working alone](../working-alone/SKILL.md).
   Open a pull request with the member run credential so Beehive can link it.
   Agree with the lead on who merges and where. Send routine questions and any
   request for a decision to the lead through the channel in your assignment.
   The lead is the contact for the person.

4. **Hand back the exact attempt.** Use a one-line `<reason>` saying why the
   attempt ended. Fill the four arrays with short sentences. In `done`, identify
   the result, pull request and checks. In `deviations`, name approved changes to
   the brief. In `concerns`, name evidence or limits the lead needs to read. In
   `followUps`, name the remaining work. Replace the example entries with those
   sentences, JSON-escaped, and use an empty array for any category with no entries.

   ```sh
   bee swarm finish-swarm-task --swarm '{"repo":{"fullName":"<repo>"},"id":"<swarm-id>"}' --task-id "<task-id>" --run-id "<run-id>" --attempt-number "<attempt-number>" --outcome "<outcome>" --reason "<reason>" --hand-back '{"done":["<result-and-checks>"],"deviations":["<scope-change>"],"concerns":["<concern>"],"followUps":["<remaining-work>"]}' --json
   ```

   Choose `finished` when the task met its brief, `empty` when no usable result
   came back, or `refused` when a rule prevented the work. The response shows the
   updated task and attempt history. If the request is stale, reread the task and
   confirm your own attempt before doing anything else. A later attempt must
   record its own result. Release your active lease as described in Working alone.

5. **Leave an empty task ready for a decision.** Say what was tried and what
   blocked a useful result. The lead can narrow the task, clear the dependency
   or arrange another attempt. Start again only after agreeing on the next
   attempt and confirming the Swarm, run and paths are available. If the runner
   already ended and Beehive closed the attempt, give the lead your evidence
   through the assignment channel. For a rules conflict or safety concern, use
   the built-in reporting flow in [When things go wrong](../when-things-go-wrong/SKILL.md).
