# Contributing to Bee

Bee accepts contributions from people and from agents operated by people. Start by reading [docs/PRODUCT.md](docs/PRODUCT.md), [SPEC.md](SPEC.md), and [docs/PLAN.md](docs/PLAN.md). They define scope, the technical contract, and path ownership.

## Set up and check a change

Run the setup script once on a development machine:

```sh
scripts/dev-setup.sh
```

Use focused checks while iterating, then run the repository gate before handoff:

```sh
cargo check --workspace
cargo test -p <crate>
cargo test -p <crate> --test integration <file>::
scripts/check.sh
```

New behavior needs tests in the crate that owns it; cross-crate behavior belongs in `bee-conformance`. New integration-test files must be included by that crate's test entry point. The pinned Rust toolchain and `protoc` are required.

## Rules for changes

Keep changes focused and deterministic. Public errors use a reason from SPEC §12.2. Workspace crates may only depend on lower layers described in `docs/PLAN.md`, and `unsafe` is forbidden. Wire changes under `proto/bee/v1/` are additive: do not renumber, reuse, or change a field type. Do not edit frozen golden vectors; add new vectors instead.

When a change alters the specification, amend the relevant contract and add a row to `docs/contracts/amendments.md`. Preserve the clean-room rule: write code, tests, fixtures, and prose for this repository rather than copying them from another project.

Use an imperative commit subject under 72 characters. Give the commit a body that explains why and include the relevant PLAN task identifier when one exists. Do not add automated-author attribution lines.

Agents follow the same rules: work only within granted authority, retain the evidence for a change, and do not use an agent to bypass review, policy, or the [Code of Conduct](CODE_OF_CONDUCT.md).

## Contribution license

Contributions are accepted under Apache-2.0 §5: unless you explicitly state otherwise, a submitted contribution is licensed under Apache-2.0 on the same terms as the project. No contributor license agreement is required, and no DCO sign-off is required.

Security vulnerabilities must use the private process in [SECURITY.md](SECURITY.md), not a public issue or pull request.
