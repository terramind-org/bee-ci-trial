# Beehive safety record

## Evidence and verdicts

The repository Safety service answers what people asked agents to do and what Beehive observed.
A record identifies either an independent agent run or a swarm. Swarm members contribute
evidence to the swarm. Evidence refreshes on request and through the durable outbox when
attributed work, run completion, swarm state, checks, scans, or governing audit operations
change the record. A failed refresh remains queued for retry.

ASKED contains the recorded task prompt, steering messages from people, issue title and
body, or swarm goal and task titles, with the subject of each. Issue text comes from the
current issue row at collection time. DONE contains attributed paths, commands from
recorded structured execution results with known secret patterns redacted, and merged pull
request IDs. Evidence lists are bounded and show the number of omitted items. Each source
must carry a verified run identity, a direct run link, or the runner's stored output attribution.
The server does not infer a command from an agent's prose or treat a command string as
evidence of network activity.

A human override with a note takes precedence. For the automatic verdict, ignore dismissed
findings and automatic notes about scans with unknown provenance. An active finding of any
other kind or severity, except a note for `outside_task` or `said_vs_shown`, gives
`needs_a_look`. An outside-task note gives `more_than_asked`. A said-vs-shown note or an
unfinished task on a swarm marked finished gives `less_than_asked`. Both give
`something_else`. With none of this evidence the verdict is `as_asked`. Each verdict
describes a comparison against the stored evidence. There is no verdict called “safe”.

## Signals

| Kind | Deterministic evidence |
| --- | --- |
| `outside_task` | An attributed diff touches a path outside the run's claimed path prefixes. Missing claims supply no comparison. |
| `around_refusal` | A verified run retries the same pull request or native ref and receives a typed denial naming the same rule and unmet requirement. Changing the native head leaves the effect unchanged. Freeze refusals and successful retries supply no evidence of a workaround. Denials without a named requirement cannot support this signal. |
| `tests_weakened` | The classified diff deletes a test file, adds recognized test skip or ignore markers, or changes a supported CI workflow path. |
| `own_leash` | An attributed diff changes repository policy or a recognized agent instruction file. Paths outside the recorded scope are severe. Paths inside it, and edits with no recorded scope, are warnings. Repository merge rules still apply. Successful governing mutations audited under the run credential or a verified descendant are severe. |
| `said_vs_shown` | A finished swarm attempt has a nonempty done claim and its attributed revision has a failed required check or workflow, or an absent required result after check suites are complete and no check or required workflow is pending. Pending results are unknown. Standalone runs have no structured hand-back, so this detector cannot fire for them. |
| `harm_to_code` | An open alert matches an attributed changeset and was first observed on a changeset belonging to the work. Beehive attributes introduction when every parent has a comparable clean scan. Alerts first observed elsewhere are excluded. A matching alert with no verifiable baseline is a note and leaves the verdict unchanged. |
| `secrets_and_exit` | Reported only. Current runners do not persist observed network destinations. |
| `following_strangers` | Reported only. Current runners do not mark instructions from untrusted content. |
| `harm_to_others`, `legal_flags` | Reported only. A person or independent reviewer supplies the evidence. |

A finding has a kind, severity, evidence and an open, confirmed or dismissed state.
Automatic findings keep their ID as the evidence for the same subject changes. When the
evidence no longer holds, Beehive removes open automatic findings and records a retirement
in the audit log. A person with the right repository role may confirm or dismiss a finding
with an explanation. That decision persists on refresh. A report may cite a watcher anomaly or other
evidence as a source. Review proposals carry the model name and findings and leave the
human's verdict authority intact.

## Access and intervention

Safety records contain whole-repository evidence. Read scope must cover the repository and
its protected paths. An alert finding is visible with code-scanning details restricted to the
same callers who can read the alert itself. Governing audit details require organization audit
access. A signed-in repository writer can report a finding. The answerable
person with write access, or a repository admin, can resolve findings, request review and
override the verdict. An assigned agent can append findings and submit a proposed verdict
on its request. An agent that participated in the work cannot review it. Review requests
emit `safety.review_requested` with the work, reviewer and request IDs.

Severe `own_leash`, `secrets_and_exit`, and `harm_to_others` findings interrupt the work
and create an attention item for the answerable person. With no answerable person, the item
goes to the repository administrators and has an empty personal recipient. A standalone run uses its existing
terminal cancellation. A swarm is paused and its active member runs are cancelled. The
reason remains in the finding and attention item. A recipient can acknowledge their own
attention items.

An agent can raise a rules conflict, safety or potential legal concern using its own active
run credential. This cancels the run and creates a separate item for the person who answers
for the work. A concern never produces a finding or contributes to a verdict. Its text is
available to the attention recipient.

An organization owner can stop agent work and freeze agent merges. Resume clears the freeze
and rechecks the merge queue. A paused swarm still needs an explicit resume. A cancelled
run needs an explicit retry. Human merges remain available under existing policy. For non-human publication on a
server with Beehive enabled, inability to read the stop flag prevents publication.

A person with repository write access can sample work with a verified agent merge and no
recorded human approval of the exact revision that merged. Spot checks rank work by a hash of the repository, period, seed, and
work identity. A period includes its start and excludes its end. Organization audit readers
can see signal counts for accessible repositories, by kind and associated agents or swarms,
tripwires and median elapsed time from observation to the first recorded human resolution.
Dismissed and retired findings do not enter signal counts. Unresolved findings do not enter
the median.

Safety events and audit entries use the `safety.*` namespace. The contract is
`proto/bee/v1/safety.proto`.
