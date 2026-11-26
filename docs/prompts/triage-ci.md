---
id: triage-ci
title: Triage failing CI
when: A workflow run or a required check is failing on a repository or change request. Find the failure, read its logs, rerun the failed jobs, and open a fix.
---
You are going to find out why CI is failing in {{repo}} on {{endpoint}}, and fix it or report why you cannot. If you have not connected yet, follow the "Set up Bee for a repository" prompt first. Over MCP, enable the CI tools once with `enable_toolset` and `{"toolset": "ci"}` (they are not in the default `core` set); the CLI needs no setup.

1. List the failing runs, newest first. Narrow by ref or by change request number when you know it:

   ```sh
   bee workflow list-workflow-runs --repo {{repo}} --conclusion failure --json
   bee workflow list-workflow-runs --repo {{repo}} --change-request-number <n> --json
   ```

   Other useful filters: `--ref`, `--event`, `--status` (`in-progress`, `completed`, ...), `--created-after`. Note each run's `id`, workflow name, ref and head changeset. If nothing fails but a required check blocks landing, list checks instead: `bee check list-checks --repo {{repo}} --changeset <changeset id> --latest-only --json`, and read `bee check list-check-annotations --help` for its annotations.

2. Find the failing job and read its log:

   ```sh
   bee workflow list-workflow-jobs --repo {{repo}} --run-id <run id> --json
   bee workflow tail-workflow-job-logs --repo {{repo}} --job-id <job id> --json
   ```

   Start from the first failing step, not the last line: later errors are often consequences. `--step-number <n>` limits the log to one step. To search every log of a run, see `bee workflow search-workflow-run-logs --help`; for a downloadable log, `bee workflow get-workflow-job-logs-url --repo {{repo}} --job-id <job id>`.

3. Decide what kind of failure it is before you change anything:
   - Your change broke it: the error names code you touched. Fix it (step 5).
   - Flaky or infrastructure: a timeout, a network error, a runner that died, a test that passes on the main branch with the same code. Rerun only the jobs that failed (step 4). Do not rerun more than twice; a third failure is a real failure.
   - Already broken on `main`: compare with `bee workflow list-workflow-runs --repo {{repo}} --ref main --json`. Say so in your report; do not "fix" it inside an unrelated change.
   - A required workflow or policy problem, for example `POLICY_DENIED` or `action-required`: it needs a person. Report it.

4. Rerun just the failed jobs of a run, or one job:

   ```sh
   bee workflow rerun-failed-workflow-jobs --repo {{repo}} --run-id <run id> --json
   bee workflow rerun-workflow-job --repo {{repo}} --help
   ```

   Add `--enable-debug-logging` to the first when you need more output. Then wait for the run with `bee workflow get-workflow-run --repo {{repo}} --run-id <run id> --json` until `status` is `completed`; do not call it in a tight loop, pause between reads.

5. Open a fix. Follow the "Open a change request and land it" prompt: take a lease on the files you change, reproduce the failure locally if you can, make the smallest change that fixes the cause, add or fix a test that would have caught it, commit with a message that names the failing job, open a change request, and read its landing evaluation. Do not weaken or delete a test, skip a check or edit the workflow just to make CI green, unless that is the actual defect and you say so. Show the fix passing: when CI proves it (a test report, a recording, a benchmark), attach that to the change request as evidence verified by CI, with `bee cr evidence add <id> --check-run <check run id> --tests <passed>:<failed>:<skipped> --summary "..."` or `--artifact <artifact id>`, rather than only saying it passes; the "Show your work on a change request" prompt has the details.

Report: the run and job ids, the failing step, the cause in one sentence, whether it was flaky or real, what you reran and the result, and the change request id of the fix if you opened one.
