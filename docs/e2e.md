# The phase 1 exit test

`scripts/e2e/exit-test.sh` runs the exit test that ends phase 1 in [`PLAN.md`](PLAN.md) as one
script. It supplies fixed inputs and checks each result through assertions. A failing
assertion identifies the operation and the expected behavior.

| # | Step | What it proves |
|---|---|---|
| 1 | `bee-github import terramind-org/bee` into `bee-server --dev` | Every commit reachable from the source's branches and tags is in the imported history under the **same git id**, every imported commit (including PR refs) matches an original GitHub id, and the fidelity report has **no unexplained drops**. |
| 2 | An agent token drives `bee mcp` | A scoped agent token (minted with `AuthService.CreateAgent` + `MintToken`) makes a change and lands it through MCP tools alone: `initialize`, `tools/list`, then `tools/call`. |
| 3 | `git clone` and `git push` against the server | A second change pushed with plain git lands on the policy-protected default branch; a push that policy forbids is refused with the SPEC §12.2 reason `POLICY_DENIED`, and the branch does not move. |
| 4 | Log, diff and blame over HTTP/JSON | Authors, creators, messages and the lines changed in steps 2 and 3 are what the history says, and blame attributes each line to the right changeset. |
| 5 | The SPEC §16 load harness | The harness runs and reports; every target it missed is printed with the measurement and by how much. |
| 6 | Restart on the same data directory | A new process serves a complete clone with every source branch/tag commit id, both landed changes, and the same log, diff and blame assertions. |

The script ends with a summary table and prints `exit-test: ok` only when all six steps passed.



The GitHub importer currently ships as `bee-github import`, the executable used here. The
SPEC spelling `bee import github` is not registered by the main CLI.

## Running it

```sh
scripts/e2e/exit-test.sh
```

Needs `git`, a Rust toolchain (pinned by `rust-toolchain.toml`), `protoc`, [bun](https://bun.sh)
and, for the import, a GitHub token that can read the source repository: `GITHUB_TOKEN` or
`GH_TOKEN`, else `gh auth token`. The token is handed to the importer and to `git` through the
environment and is never printed. The script builds `bee`, `bee-server`, `bee-github`
and the load harness in release mode (the first build takes a while), starts
`bee-server --dev --temp` on a free loopback port, runs the steps, stops the server and removes
its scratch directory. The generated server config sets `import.github_sync_interval_secs = 0` for a snapshot import followed by local lands; live-sync and bridge behavior have separate drills. Builds use `/tmp/bee-build.lock` (`lockf` on macOS, `flock` on Linux), disable incremental compilation and dev debug information, and refuse to start with less than 10 GiB free. Step 6 stops the first server and opens the same SQLite and object-store
directory in a new process before checking history again.

| Variable | Default | Meaning |
|---|---|---|
| `GITHUB_TOKEN`, `GH_TOKEN` | `gh auth token` | GitHub token for step 1. |
| `EXIT_TEST_REPO` | `terramind-org/bee` | Source repository, `owner/name`. Point it at a small public repository for a quick run. The server gets an org named after the owner. |
| `EXIT_TEST_IMPORT_TIMEOUT_SECS` | `3600` | How long step 1 waits for the import. |
| `EXIT_TEST_LOAD_ARGS` | see below | Workload flags for the load harness. |
| `EXIT_TEST_PROFILE` | `release` | `release` builds and runs optimized binaries. Set to `dev` to build and run debug-profile binaries when a release build is too slow; the scenarios still run, but its load figures are not performance measurements. Any other value is an error. |
| `EXIT_TEST_SKIP_IMPORT` | unset | `1` is the offline mode, below. |
| `EXIT_TEST_KEEP` | unset | `1` keeps the scratch directory (server log, clones, `state.json`, the load report) and prints its path. |
| `CARGO_BUILD_JOBS`, `CARGO_TARGET_DIR` | `6`, cargo's target directory | Cargo build settings. |

Exit status: `0` all six passed, `1` a step failed (the summary names it), `3` the offline mode
ran. Without `docker`, `kind` or any service, nothing else is needed: the server is the dev
server, SQLite and local disk.

### Offline mode

`EXIT_TEST_SKIP_IMPORT=1` skips step 1, so the script runs without a GitHub token or GitHub access. The initial build may download dependencies. Steps 2 to 4 and 6 then use the dev server's own `dev/playground`. Step 1 is reported
**INCOMPLETE**, the final line is `exit-test: INCOMPLETE …` and the exit status is `3`: an offline
run is a smoke check of the rest, never a pass of the exit test.

## What each step checks

**Step 1.** A fresh `git clone --mirror` gives the source branch/tag commit list and all
GitHub ids reachable through its advertised refs. `bee-github import` asks the server
for an import through `ImportService.StartImport`; the script waits for `GetImport` to
report live. A mirror clone of the imported repo fetches all advertised refs, including PR
history. Every source branch/tag commit must retain its id, and the full clone's commit
count must match the fidelity report. Every imported git id must exist in the source
mirror; an id that arrived later is fetched directly from GitHub for independent
verification. The source default-branch tip must remain an ancestor of the imported tip.

The fidelity-report checker in [`lib/fidelity.ts`](../scripts/e2e/lib/fidelity.ts) verifies
that each dropped count agrees with its listed drops and that every drop has a reason.
No commit may be dropped. Other explained drops are printed with their reasons. The
report follows `schemas/v1/import-report.schema.json`.

**Step 2.** The agent's token is scoped to the repository by its **full name** (`read`, `commit`, `land`, `review`,
`lease`) with the session `exit-test-mcp`. `bee mcp --toolsets core` is spawned with only
`BEE_ENDPOINT` and `BEE_TOKEN`; the script resolves the default branch, uploads a blob,
creates a changeset and lands it with `object_resolve_ref`, `write_upload_objects`,
`write_create_changeset` and `write_land`. The land is then checked through the API with the admin
token: the ref advanced, the tip is a one-file change made by the agent with the token's session in
its provenance, and the file has the text the agent wrote.

**Step 3.** The pusher is the dev server's human user. Before the first push the script creates a
ruleset on the default branch that restricts `secrets/**`. The clone (without a working-tree
checkout, so a large repository costs nothing) contains step 2's file. A commit that adds a line to
it and adds a note is pushed to the default branch: git reports success, the API shows the branch at
a descendant of step 2's change, `git ls-remote` reports the pushed commit, and the ruleset's
evaluation log has a passing entry. A second commit that adds `secrets/exit-test.txt` is pushed:
git must report a rejection that names `POLICY_DENIED`, the branch must not move, and the log must
hold an evaluation that did not pass.

**Step 4.** Log (first-parent page, path filter, author filter), diff (between the commits before and
after each change, with hunks, statuses and added or removed lines) and blame (whole file, another
file, a line range) over `POST /bee.v1.QueryService/…`, asserting on the text and ids of the
two changes.

**Step 5.** The `load` example of `bee-conformance` (see
[`benchmarks.md`](benchmarks.md#how-to-run)) runs against its own in-process server and writes the
SPEC §16 table. The script prints the table, then one `MISSED` line per target that was missed with
its target, p50, p99 and the overshoot. The step passes when the harness runs to the end and every
target has a measurement; each miss is printed with its margin. Misses do not fail this reporting step. The default
workload is the full one the benchmark page describes for SQLite (`--landers 1000 --samples 100
--sessions 10000 --files 10000`) because SPEC §16's targets name those sizes: a smaller workload
would leave the stated 10,000-session and 1,000-lander workloads untested. Lower it with `EXIT_TEST_LOAD_ARGS` for a faster, less faithful run. Measurements and misses for the recorded local runs appear below; [`benchmarks.md`](benchmarks.md) tracks the broader performance work.

**Step 6.** A separate server process opens the original data directory. A fresh mirror clone must
pass `git fsck --full`, contain both landed changes, and retain every imported git id, including PR history. The complete step 4 assertions run again against the restarted process.

## Policy and agents

Step 3 uses the dev server's human user as its pusher. A ruleset created through
`PolicyService.CreateRuleset` has no `agents_may_land_without_human` list, and SPEC §9.4 reads an
unset list as "agents always need a human approval". An agent that tries to land on a branch under
such a ruleset is refused with `POLICY_DENIED` (`missing: approval:human`) until a human approves
its change request. Step 2 lands as an agent where no ruleset applies and the built-in land rule
lets agents land every class.

## In CI

[`.github/workflows/e2e.yml`](../.github/workflows/e2e.yml) runs the script on `ubuntu-24.04` for
every pull request and every push to `main`. The import source is this repository, read with the
workflow's `GITHUB_TOKEN` (`contents`, `issues` and `pull-requests`, all read-only). The helpers' unit tests
(`bun test scripts/e2e/test`) run first.

## Files

| Path | Role |
|---|---|
| `scripts/e2e/exit-test.sh` | Build, start the server, run the steps, print the summary. |
| `scripts/e2e/step1-import.ts` … `step6-restart.ts` | One step each; exit `0` pass, `1` fail, `3` incomplete. |
| `scripts/e2e/lib/rpc.ts` | Connect (HTTP/JSON) client on the agent harness's, shared state, helpers. |
| `scripts/e2e/lib/mcp.ts` | Scripted MCP client over `bee mcp` stdio. |
| `scripts/e2e/lib/fidelity.ts`, `lib/loadreport.ts` | Report parsers, with tests in `scripts/e2e/test/`. |

## Reverted-tree regression

The real-repository clone uncovered a fetch-pack omission: a history whose root trees are
A, B, A could fail with `did not receive expected object`. The tree walker compares each tree
with its parents and skips identical entries. In child-first order, the first visit to A
skipped content shared with B, B skipped it against the older A, and the older A was then
considered visited. Shared objects were absent from the pack even though `ReadPath` could read
them from storage.

`bee-git-http` now walks sent commits' trees in ancestor-first generation order. Each
parent's promised objects have been visited before a child may skip them. The regression
`git_cli::a_history_that_returns_to_an_earlier_tree_clones_in_full_and_shallow` imports three
commits with a changing file and an unchanged sibling, reopens the durable store, reads the
sibling through the API, then checks both a full clone and a depth-two clone with git fsck.
Against the original walker it failed at the full clone in 0.76 seconds after compilation.
With the fix, the same test passed in 0.66 seconds (one test, zero failures). The
2026-11-15 run used an Apple M4 Pro, 14 cores, 24 GiB RAM; the adjacent exit-test run
recorded load averages 20.70 / 45.27 / 43.75. Compilation took 5m01s for the original
run and 1m37s for the fixed dependency rebuild, with Cargo incremental
compilation disabled in both.


## Local verification, 2026-11-15

The local default-backend gate ran on an Apple M4 Pro (14 cores, 24 GiB RAM), with
`CARGO_BUILD_JOBS=6`, `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=0`.
`lockf /tmp/bee-build.lock scripts/check.sh` passed in 2463.21 seconds including
lock wait: formatting, clippy, 2,376 passing Rust tests/doc-tests, layers, pins, coverage,
protoc, web lint/typecheck and 492 passing web unit tests across 52 files.
Four Rust tests/doc-tests were ignored by their existing annotations: the public import
corpus, the release-only secret-scanning timing test, the Postgres hot-ref benchmark,
and a service-module documentation example.

The exit runner's eight helper tests passed (15 assertions, 149 ms), its TypeScript
check passed, and `bash -n scripts/e2e/exit-test.sh` passed. Workflow lint passed with
both the native actionlint 1.7.7 binary and `rhysd/actionlint:1.7.7` in Docker. Docker
initially timed out; the later container run exited zero.


## Mirror polling and the restart check

The first full local run passed steps 1 through 5, then failed because `main` had lost the
agent and git-pushed files. The durable ref log showed version 1 from the importer,
version 2 from the agent, version 3 from the pusher, and version 4 from the importer
pointing back to version 1's target. The default 300-second GitHub poll had run during
the 255-second load drill. Mirror mode follows GitHub's refs (SPEC §17.5).

The runner now uses the supported `github_sync_interval_secs = 0` setting for both server
processes. This keeps the imported snapshot stable while the local land and durability
assertions run. The tree-walk regression above required a product fix; this ref reset
required isolating the snapshot test from live reconciliation.

That first run took 3387 seconds including build and lock waits. Its step durations were
170 / 1 / 7 / 0 / 255 / 10 seconds, with only the final restart step failing. The load
report measured seven hits, three misses and one monthly SLO without a verdict. The
misses were CreateChangeset (p50 21.10 ms, p99 621.87 ms at 11,200 files), event delivery
(p50 634.32 ms, p99 2467.04 ms), and 9,997 fully receiving sessions out of 10,000.
The load average was 6.18 before and 13.78 after that harness run on the machine above.


## Passing snapshot run, 2026-11-15

The revised script passed all six steps against `terramind-org/bee` in 574 seconds.
The source branch/tag snapshot held 665 commits. The complete imported mirror held all
670 commits named in the fidelity report, including PR refs; each id existed in the
independent source mirror. All 670 survived a server restart. The fidelity report had
28 categories, 19,845 imported records, 64 archived records and zero drops.
The latest helper rerun passed eight tests and 15 assertions in 128 ms.

```text
step   name                                 result      seconds
1      import github history                PASS        184
2      agent change over MCP                PASS        1
3      git clone and push under policy      PASS        8
4      log, diff and blame                  PASS        0
5      load harness (SPEC §16)             PASS        295
6      restart, clone and API history       PASS        11
total time: 574s (9m 34s)
exit-test: ok
```

The report below is from the passing script run. Three performance targets still miss;
the reporting step passes when it measures and exposes every target.

- Date: 2026-11-15 00:32 UTC
- Machine: Apple M4 Pro, 14 cores, 24 GiB, macos
- Store: SQLite (WAL, synchronous = FULL, F_FULLFSYNC on macOS); blobs on local disk
- Build: release
- Load average (1 min): 10.23 before, 11.65 after
- Workload: 100 samples per latency row, 1000 landers, 10000 subscriptions, repo grown by 10000 files
- One in-process server, gRPC over loopback; latencies are client-observed

| Operation | Target | p50 | p99 | Measured | Result | Notes |
|---|---|---|---|---|---|---|
| CreateChangeset, 1 file changed, server time | p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size | 22.18 ms | 81.46 ms | 100 samples | **miss** (by 122 %) | repo of 11200 files; in a 200-file repo: p50 11.70 ms, p99 19.52 ms |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms, p99 ≤ 150 ms | 6.94 ms | 14.37 ms | 100 samples | hit (77 % margin) |  |
| Land throughput, one hot ref, 1,000 concurrent landers | ≥ 100 changes/s, every lander lands within 10 s p99 | 3531.39 ms | 4608.55 ms | 216 lands/s; 1000 landers | hit (54 % margin) | 4 CAS batches (avg 250.0 lands per batch); wall 4.62 s; max time-to-land 4613.81 ms |
| Sparse checkout, 1,000 files, warm node cache | ≤ 1 s | 157.64 ms | 157.64 ms | 1 samples | hit (84 % margin) | bee-wc clone through a node daemon with a warm disk cache |
| Sparse checkout, 1,000 files, cold, same region | ≤ 3 s | 276.19 ms | 276.19 ms | 1 samples | hit (91 % margin) | bee-wc clone straight from the server (loopback), empty working copy |
| Log via API, 100-entry page | p99 ≤ 200 ms | 0.99 ms | 4.73 ms | 100 samples | hit (98 % margin) | under 32 mixed readers: p50 4.50 ms, p99 8.50 ms |
| Blame via API, one file of 10k lines | p99 ≤ 200 ms | 2.98 ms | 13.15 ms | 100 samples | hit (93 % margin) | 10,000 lines, 50 edits; under 32 mixed readers: p50 46.20 ms, p99 59.95 ms |
| Diff via API, one file of 10k lines | p99 ≤ 200 ms | 2.62 ms | 5.39 ms | 100 samples | hit (97 % margin) | 10,000-line file with hunks; under 32 mixed readers: p50 11.69 ms, p99 19.02 ms |
| Event delivery, commit to subscriber | p99 ≤ 1 s | 3438.69 ms | 25494.74 ms | 10000 streams open | **miss** (by 2449 %) | 49966 deliveries of 5 lands to 10000 subscribers; 9981 subscribers got all |
| Concurrent agent sessions with streams open | ≥ 10,000 per cell | – | – | 9981 streams open | **miss** (by 0 %) | 10000 subscriptions attempted over 100 connections by 100 agent tokens; 10000 opened, 0 refused; counted: those that received every event |
| Availability | reads 99.99%, writes 99.95%, per cell, per month | – | – | observed reads 100.0000 %, writes 100.0000 % over 2345 requests | not judged: a monthly SLO; one run reports its observed success ratios | every harness request |

| Extra measurement | Value |
|---|---|
| Forge event log after the 1000-lander burst (polled every 200 ms) | burst's last land in the log after 30.03 ms; a land made right after it after 30.03 ms; log held main up to version 1108 of 1108 |
| Mixed reads (log, blame, diff, list tree, resolve), 32 concurrent readers | 2126 reads/s |
| git clone over smart HTTP (12450 files, 15.2 MiB packed) | p50 1619.98 ms (9.4 MiB/s, 6790 files/s) |
| Code search, literal queries over 11000 files (no SPEC §16 bound) | p50 1.84 ms, p99 3.61 ms (100 queries); index caught up in 0.2 s |

On 2026-11-15 the PR workflow failed before executing any steps in
[run 43658947260](https://github.com/terramind-org/bee/actions/runs/43658947260).
The check annotation identified failed account payments or an insufficient spending limit.
The commands and pinned actionlint container passed locally; CI execution remains unverified.
