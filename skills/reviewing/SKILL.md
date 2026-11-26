---
name: reviewing
description: Use this when asked to independently review someone else's pull request or submit a Beehive safety review.
---

# Reviewing

1. **Identify the revision and check independence.** Get `<repo>` and the
   `<pull-request-id>` from the review assignment or the pull request's operation
   details in Beehive. Use the internal ID from those details.

   ```sh
   bee change get-change --repo "<repo>" --id "<pull-request-id>" --json
   bee brief list-evidence --repo "<repo>" --change-ids "<pull-request-id>" --json
   ```

   The first response gives the author, brief and revisions. The second lists ready
   evidence and the revision each item was collected for. If you made the change
   or participated in the work under safety review, pass it to an independent
   reviewer. Beehive refuses self-approval and self-assigned safety review. If a
   read fails, check the ID and repository access before drawing conclusions.

2. **Compare the brief, files and evidence.** Read the issue and the pull request's
   Files changed view for the current revision. Start with what the person will
   see. Can they tell why the change matters, what decision needs them, which
   checks ran, and which files deserve attention? Look at the whole change, including
   files outside the highlighted paths. Check that the brief and evidence cover
   this revision. Treat a screenshot or test report labeled as supplied by the
   agent as the author's evidence. Check which results CI verified.

   Use the latest `data.revisions` entry's `head.raw` for
   `<revision-head-base64>`.

   ```sh
   bee check list-checks --repo "<repo>" --changeset '{"raw":"<revision-head-base64>"}' --latest-only --json
   ```

   This reports the latest check per name for the revision head. Missing checks
   and stale evidence are gaps to report. If the revision changes during review,
   confirm the new head and review the relevant changes before submitting.

3. **Submit a pull request review.** Use the current entry's `n` for `<revision>`.
   Choose `approve`, `request-changes` or `comment` as `<verdict>`, and write
   `<review-note>` naming the result, impact and any required fixes. Anchor file
   findings in Beehive on the line that demonstrates the problem.

   ```sh
   bee change submit-review --repo "<repo>" --change-request-id "<pull-request-id>" --revision "<revision>" --verdict "<verdict>" --body "<review-note>" --json
   ```

   This returns the recorded review. Policy decides whether your approval counts.
   If the review is refused on the current revision, report the reason and hand it
   to an eligible independent reviewer.

4. **Read the safety record for the work behind the change.** Get `<work-id>` from
   the linked Agent run or Swarm page, and use `run` or `swarm` for `<work-kind>`.

   ```sh
   bee safety get-safety-record --repo "<repo>" --kind "<work-kind>" --id "<work-id>" --json
   ```

   This returns what was asked, what Beehive observed, findings, reviews and the
   person who answers for the work. Compare the recorded brief and steering with
   the observed paths, commands and merged pull requests. Empty evidence can mean
   the runner did not capture it. A verdict describes the fit between the ask and
   the evidence. If protected paths are outside your read scope, tell the requester
   which evidence you could not inspect.

5. **Request independent safety review through the person.** The answerable person
   with repository write access, or a repository admin acting as a person, can
   request it. Give them the work reference and `<reviewer-id>` from the repository
   organization's agent directory. In a Swarm, send the request through the
   Queen · lead. Pick an agent that did not participate.

   ```sh
   bee safety request-safety-review --work '{"repo":{"fullName":"<repo>"},"kind":"<work-kind-enum>","id":"<work-id>"}' --reviewer-id "<reviewer-id>" --json
   ```

   For nested JSON, use `SAFETY_WORK_KIND_RUN` or `SAFETY_WORK_KIND_SWARM` as
   `<work-kind-enum>`. This returns the request and assigned reviewer. If the call
   returns `safety_self_review`, send the person a different reviewer ID.

6. **Report a concrete finding and submit the safety review.** As the assigned
   agent, take `<review-id>` from the request or its entry in `data.reviews`.
   Name the observation and cite the relevant path or subject. This example
   reports an out-of-scope path. Use the matching signal and a severity supported
   by the evidence. Escape quotes and newlines in the nested JSON.

   ```sh
   bee safety report-finding --work '{"repo":{"fullName":"<repo>"},"kind":"<work-kind-enum>","id":"<work-id>"}' --review-id "<review-id>" --report '{"kind":"SAFETY_SIGNAL_KIND_OUTSIDE_TASK","severity":"SAFETY_SEVERITY_WARNING","evidence":{"observation":"<observation>","paths":["<repository-relative-path>"]}}' --json
   ```

   The call returns the finding ID and state. A person with repository write
   access can report without a review ID. When the assigned review is ready,
   supply `<review-model>` from your runtime, a `<review-note>` summarizing
   evidence and gaps, and one proposed verdict. Use `as-asked`, `more-than-asked`,
   `less-than-asked`, `something-else` or `needs-a-look`.

   ```sh
   bee safety submit-safety-review --work '{"repo":{"fullName":"<repo>"},"kind":"<work-kind-enum>","id":"<work-id>"}' --review-id "<review-id>" --model "<review-model>" --proposed-verdict "<proposed-verdict>" --note "<review-note>" --json
   ```

   The response gives the submitted proposal and finding IDs. The person keeps
   authority over any override. If a write fails, check the assigned reviewer,
   request ID and submission state. Keep the evidence for the authorized reviewer
   if you cannot submit it yourself.
