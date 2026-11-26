# Working on Bee and Beehive

Bee is source control built for AI agents; Beehive is the forge on top of it, the way GitHub sits
on git. Both live in this repository. Most contributors here are agents, so these rules are
written for you.

## Read first

0. `docs/PRODUCT.md` is the scope and the definition of done. A feature is done when agents
   pass its scenarios (§3.1) and its row in `docs/coverage/features.csv` has a passing test.
1. `SPEC.md` is the contract. If your change disagrees with it, amend the spec in the same
   change and log it in `docs/contracts/amendments.md`.
2. `docs/PLAN.md` says which task owns which paths. Edit only the paths your task owns.
3. `proto/bee/v1/` is the wire contract. Changes are additive only: never renumber or reuse
   a field, never change a field's type.

## Naming

There are two names and both are user-facing. Use each the way people use "git" and "GitHub".

- **Bee** is the version control system and its tools: the `bee` CLI and its help text, the
  object model, changesets, refs, landing, rebase and merge, the wire protocol (`bee.v1`), the
  daemon and the working copy, the `.bee` reserved path, "a Bee repository", import from git.
- **Beehive** is the forge and everything hosted: the web app, accounts, sign-in and tokens,
  organisations, issues, change requests and reviews, discussions, releases, packages, pages,
  workflow runs, webhooks, emails and notifications, the GitHub-compatible API, and a server or
  instance as the place repositories live ("this Beehive server", "self-host Beehive").
- When a sentence is about both, name both: "push your Bee repository to Beehive".

Identifiers do not follow this rule and do not change: crates and binaries (`bee-server`,
`bee-forge`), env vars (`BEE_*`), headers (`X-Bee-*`), the proto package, paths and URLs keep
the `bee` spelling whichever layer they belong to. Write each name as one word with one capital:
never "BeeHive", "Bee Hive" or "BeeHub".

## Commands

```sh
scripts/dev-setup.sh   # once per machine: toolchain, protoc, bun
cargo check --workspace          # fast feedback
cargo test -p <crate>            # while iterating: only the crate you touched
cargo test -p <crate> --test integration <file>::   # one file under tests/
scripts/check.sh                 # before handing off: fmt, clippy, tests, layer check, protoc
```

Toolchain is pinned in `rust-toolchain.toml`. `protoc` must be on PATH.

Each crate's integration tests build as one test program, `integration`, from `tests/main.rs`,
which lists every file under `tests/` as a module (so the crate is linked once, not once per
file). A new test file needs a `#[path = "<file>.rs"] mod <file>;` line there; helpers shared by
several files are declared once in `main.rs` and imported with `use crate::<helper>;`.
Exception: `bee-server` keeps one test program per file. Each of its tests starts complete
servers (async runtime, SQLite and blocking-pool threads), and four of its files together already
peak above 4,000 threads, against macOS's limit of 6,144 per process.

## Rules

- **Clean room.** We read other projects to learn, but never copy their code, tests, fixtures
  or prose into this repository. Write it yourself from the spec.
- **No new dependency without a reason** written in the commit message. Pin versions in the
  workspace `Cargo.toml`.
- **Determinism.** Encoding, rebase, merge, derived data and diff classification must be pure
  functions of their inputs. No wall-clock time, randomness or hash-map iteration order in
  anything that affects an id.
- **Errors are contract.** Return a SPEC §12.2 reason, never a bare string, from anything a
  client can see.
- **Tests come with the change.** Unit tests in the crate; cross-crate behaviour in
  `bee-conformance`; frozen golden vectors are never edited, only added.
- **Layers.** A crate depends only on lower layers (`docs/PLAN.md`). `scripts/check.sh` enforces it.
- `unsafe` is forbidden workspace-wide.

## Commits

Imperative subject under 72 characters, a body that says why, and the PLAN task id (e.g. `F1`)
when there is one. No agent attribution lines.
