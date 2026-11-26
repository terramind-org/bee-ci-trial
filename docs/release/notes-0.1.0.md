# Bee and Beehive 0.1.0 release notes

## What is included

0.1.0 is the first pre-release of Bee and Beehive. It includes the first three build steps in PRODUCT §6: server-side version control; the API, CLI, MCP, events, tokens, and policy; Git access and import; and change requests, workflows, search, leases, and agent-runner foundations. It can be self-hosted using the documented deployment stack.

## Known gaps

This release does not meet the full PRODUCT §3.1 agent-scenario bar: the published report has one non-isolated Claude Code runtime, with 15 of 39 scenarios passing 3/3, and no Codex results. Event delivery at 10,000 subscribers missed the SPEC §16 target in the latest run: p99 was 2.5 to 25.5 s and 3 to 19 sessions missed events. An intermittent linearizability failure is under investigation, and signing-key rotation across replicas is being fixed.

GitHub Actions has not run for this repository because account billing prevented jobs from starting. A mirrored repository with the default 300 s poll can have its local `main`, including local lands, replaced by the upstream tip. The hosted edition is not ready for a public beta; see the hosted audit before attempting a hosted launch.

## Upgrade notes

Rulesets created through the API no longer add an implicit human approval. Humans are required only when the rule sets `human_count`. On upgrade, an existing API-created ruleset stops requiring a human unless it sets `human_count`; review those rulesets before relying on an approval requirement.
