---
name: rules-and-refusals
description: Use this to understand a Beehive policy decision, respond to a refusal, or propose a change to repository rules or agent instructions.
---

# Rules and refusals

1. **Read the rules in effect.** Use `<repo>` and `<target>` from the assignment or
   pull request. For a pull request, find its internal `<pull-request-id>` in
   Beehive or in the response that opened it.

   ```sh
   bee policy get-effective-policy --repo "<repo>" --ref "<target>" --json
   bee change evaluate-landing --repo "<repo>" --change-request-id "<pull-request-id>" --json
   ```

   The first call returns the effective policy. The second evaluates the current
   pull request and reports whether it can merge, which requirements were met, and
   what remains. Use the rule, revision and ref from the response. If either read
   fails, check access and the target before acting on any assumptions.

2. **Understand why the answer can differ by task.** Your repository role and
   token scope limit which actions, paths and branches you can use. Delegation
   can only narrow authority. Repository policy overlays organization defaults
   with `.beehive/policy.toml` at the tip of the target branch. Matching rules
   may require checks, approvals by specific people, leases or a merge queue.
   A draft policy takes effect after it merges. The rules already in force govern its merge.

3. **Read and respect a refusal.** In JSON output, read `error.reason`,
   `error.payload.subcode` if present, and the typed payload. Keep the request ID
   if one is provided. The reason identifies the failure class. The subcode and
   details identify the specific gate. A policy denial can list missing checks,
   approvals or a path rule. Wait for or satisfy the stated requirement through
   the authorized workflow. Check the current decision again before a merge.

   Never retry a refused action by another route, account, ref or transport.
   Trying the same merge effect after a refusal can be recorded as
   `around_refusal`. For a transient error marked retryable, wait for any supplied
   delay and repeat only the same authorized request. If the outcome may already
   have been recorded, read the current state first.

4. **Ask for a rule change through a pull request.** Give the person the rule
   and reason that stopped the work, the change you propose, its scope and what
   could be affected. In a Swarm send this through the Queen · lead. Follow the
   person's authorization and repository rules for editing `.beehive/policy.toml`
   or the applicable repository instruction file. Keep the rule change separate
   from the blocked work.

   For a policy proposal, write the candidate TOML to an authorized local file.
   Use that file's local path for `<candidate-policy-file>` and the branch it
   would govern for `<target>`.

   ```sh
   bee policy validate-policy --repo "<repo>" --ref "<target>" --toml "$(cat '<candidate-policy-file>')" --replay-sample 10 --json
   ```

   This prints validation issues and, for valid policy, the proposed rules and a
   replay against recent pull requests. Correct reported errors before review.
   Use the pull request workflow in [Working alone](../working-alone/SKILL.md).
   Include who gains or loses authority, the replay results and the person's
   required decision. If your scope excludes the rule files, hand the proposal
   to the person who can sponsor the change and leave the blocked work stopped.
