# Beehive safety record

## Evidence and verdicts

The repository Safety service answers what people asked agents to do and what Beehive observed.
A record identifies either an independent agent run or a swarm. Swarm members contribute
evidence to the swarm. Evidence is refreshed on request and when attributable work, agent
run completion, or pull requests produce relevant events.

ASKED contains the recorded task prompt, steering messages from people, issue title and
body, or swarm goal and task titles, with the subject of each. Issue text comes from the
current issue row at collection time. DONE contains attributed paths, commands from
recorded structured execution results with known secret patterns redacted, and merged pull request IDs. Each source must carry
a verified run identity, a direct run link, or the runner's stored output attribution.
The server does not infer a command from an agent's prose or treat a command string as
evidence of network activity.

A human override with a note takes precedence. For the automatic verdict, ignore dismissed
findings. Any active finding other than a note for `outside_task` or `said_vs_shown` gives
`needs_a_look`. An outside-task note gives `more_than_asked`. A said-vs-shown note or an
unfinished task on a swarm marked finished gives `less_than_asked`. Both give
`something_else`. With none of this evidence the verdict is `as_asked`. Each verdict
describes a comparison against the stored evidence.

## Signals

| Kind | Deterministic evidence |
| --- | --- |
| `outside_task` | An attributed diff touches a path outside the run's claimed path prefixes. Missing claims supply no comparison. |
| `around_refusal` | A verified run tries to merge the same pull request, or land the same native ref and head, again after a policy or approval refusal. The effect is identified using structured observations. |
| `tests_weakened` | The classified diff deletes a test file, adds recognized test skip or ignore markers, or changes a supported CI workflow path. |
| `own_leash` | The run changes repository policy or recognized agent instructions in an attributed diff, or its credential is recorded on a successful audited rule, permission, budget or credential change. |
| `said_vs_shown` | A finished swarm attempt has a hand-back with a nonempty done statement for a revision whose required checks or workflows failed or have no satisfying result. |
| `harm_to_code` | An open code scanning alert has an open instance on the exact attributed changeset. |
| `secrets_and_exit` | Reported only. Current runners do not persist observed network destinations. |
| `following_strangers` | Reported only. Current runners do not mark instructions from untrusted content. |
| `harm_to_others`, `legal_flags` | Reported only. A person or independent reviewer supplies the evidence. |

A finding has a kind, severity, evidence and an open, confirmed or dismissed state. Identical
automatic evidence has a stable finding ID. A person with the right repository role may
confirm or dismiss it with an explanation. A report may cite a watcher anomaly or other
evidence as a source. Review proposals carry the model name and findings and leave the
human's verdict authority intact.

## Access and intervention

Safety records contain whole-repository evidence. Read scope must cover the repository and
its protected paths. A signed-in repository writer can report a finding. The answerable
person with write access, or a repository admin, can resolve findings, request review and
override the verdict. An assigned agent can append findings and submit a proposed verdict
on its request. An agent that participated in the work cannot review it. Review requests
emit `safety.review_requested` with the work, reviewer and request IDs.

Severe `own_leash`, `secrets_and_exit`, and `harm_to_others` findings interrupt the work
and create an attention item for the answerable person. A standalone run uses its existing
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
recorded human approval. Spot checks rank work by a hash of the repository, period, seed, and
work identity. A period includes its start and excludes its end. Organization audit readers
can see signal counts for accessible repositories, by kind and associated agents or swarms,
tripwires and median elapsed time from observation to the first recorded human resolution.
Unresolved findings do not enter the median.

Safety events and audit entries use the `safety.*` namespace. The contract is
`proto/bee/v1/safety.proto`.
