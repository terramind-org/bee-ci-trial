# Bee and Beehive

Bee is open-source source control built for AI agents, and Beehive is the forge on top of it, the way GitHub sits on git. Bee keeps history, merges, and landing on the server, lets clients fetch only the objects they need or use the API directly; Beehive adds review, checks, leases, provenance, audit records, and event streams so many agents can work on the same repository without silently colliding.

## Status

Bee and Beehive are a pre-release 0.1.0. The open-source release covers build steps 1 through 3 in the [product scope](docs/PRODUCT.md): server-side version control, API, CLI, MCP, Git access and import, reviews, workflows, search, leases, and agent-runner foundations. It is self-hostable, but it is not yet a production-ready hosted service; hosted operation is planned without a date.

The first published agent-scenario run covered one non-isolated Claude Code runtime. It recorded 72 runs, 51 passes, and 15 of 39 scenarios passing 3/3; Codex was pending, so no scenario meets the required two-runtime bar. See the exact results and limitations in [the scenario report](docs/release/scenarios.md).

Known gaps include event delivery at 10,000 subscribers (p99 was two and a half to twenty-five and a half seconds against the one-second target, with three to nineteen sessions missing events), an intermittent linearizability test failure under investigation, signing-key rotation that is being fixed across replicas, and GitHub Actions workflows that have not run because of account billing. With the default five-minute mirror poll, a mirrored repository's local `main`, including local lands, is replaced by the upstream tip. The [0.1.0 release notes](docs/release/notes-0.1.0.md) record these and other known gaps.

## Quick start

For a development stack, install Docker and run:

```sh
docker compose -f deploy/compose.dev.yml up --build
```

The full self-hosting guide covers configuration, production deployment, backup, and restore: [docs/self-hosting.md](docs/self-hosting.md). Install release binaries using [docs/install.md](docs/install.md). A hosted service is planned, with no announced date; deployment planning is in [docs/hosting.md](docs/hosting.md).

## Using Bee and Beehive with agents

Agents can work through the MCP server, the CLI's stable `--json` output, or the native API. The [agent prompts](docs/prompts/) show tasks such as opening and landing a change, holding a lease, and self-hosting; the [agent guide](docs/agents-guide.md) describes the interfaces.

## Performance

These are single-machine measurements, not production numbers. The benchmark report measured event-delivery p99 of 655.24 ms on SQLite and 663.43 ms on Postgres with 10000 streams open in its 2026-10-20 run; later measurements exposed the open event-delivery gap described above. Read the methods, machine, workload, and complete results in [docs/benchmarks.md](docs/benchmarks.md).

## Repository layout

| Path | Contents |
|---|---|
| `SPEC.md` | Technical contract |
| `proto/bee/v1/` | Wire API definitions |
| `schemas/v1/` | JSON schemas for CLI output |
| `crates/` | Rust server, CLI, MCP server, and libraries |
| `apps/web/` | Web application |
| `deploy/` | Compose, Helm, and infrastructure configuration |
| `docs/` | Product scope, operations guides, benchmarks, and release evidence |
| `harness/agents/` | Agent-scenario harness |

## References

Read the [technical specification](SPEC.md), [product scope](docs/PRODUCT.md), [self-hosting guide](docs/self-hosting.md), [scaling guide](docs/scaling.md), [end-to-end guide](docs/e2e.md), and [scenario report](docs/release/scenarios.md). Contributions follow [CONTRIBUTING.md](CONTRIBUTING.md); security reports follow [SECURITY.md](SECURITY.md). Bee is licensed under [Apache-2.0](LICENSE).
