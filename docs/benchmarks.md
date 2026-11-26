# Benchmarks: SPEC §16 at agent scale

This page is the report SPEC §16 names: every performance target with the value the load
harness measured, whether it was met and by what margin, the machine, its load average and
the store. It also records the complexity gates (SPEC §14.6), the correctness-at-scale runs
(linearizability, many-agent simulation, faults) and what the production gate G1 still needs.

**These are single-machine numbers, not production numbers.** The machine was shared with
other build and test jobs during every run; the load average is recorded with each run and
was far above idle. Latencies on a quiet machine would be lower; the counted complexity gates
are unaffected by load.

## How to run

```sh
# Release build of the harness (an example of bee-conformance: it starts an in-process
# bee-server, which is a higher layer than the conformance crate).
cargo build --release -p bee-conformance --example load
target/release/examples/load --landers 1000 --samples 200 --sessions 10000 --files 10000 \
    --out report.md
# Postgres: start a throwaway primary + streaming standby, then pass --pg (the forge's
# tables go to the same Postgres; --forge-sqlite keeps them in SQLite instead).
crates/bee-store-sql/scripts/pg-test-cluster.sh run target/release/examples/load --pg \
    --landers 1000 --samples 200 --sessions 10000 --files 10000
```

The harness (`crates/bee-conformance/src/load/main.rs`) drives one in-process server over
gRPC on loopback, the way agents do, and renders `bee_conformance::load::report`: each
SPEC §16 row judged against the measurement (hit with the smallest headroom, or miss with the
largest overshoot), plus extra measurements SPEC §16 does not bound. Workloads, in order:

1. One-file `CreateChangeset` (blob uploaded first), 200 samples, in a small repo.
2. Uncontended `Land` of a one-change stack, 200 samples.
3. 10,000 event subscriptions (`EventService.Subscribe`) opened by 100 agent tokens over 100
   connections and held open while 5 lands are delivered to every one of them; delivery is
   timed from the land request to the subscriber.
4. The repo grows by 10,000 files plus a 1,000-file sparse prefix; `CreateChangeset` again.
5. 1,000 concurrent landers on `main` (the land coalescer), each with its own change on the
   same tip, over 16 connections; time-to-land per lander and lands per second over the
   wall clock; then the delivery time of one event right after that burst.
6. A 10,000-line file edited 50 times; log (100-entry page), blame and diff, 100 samples
   each, then the same reads under 32 concurrent mixed readers.
7. Sparse checkout of the 1,000-file prefix through `bee-wc`: cold straight from the
   server into an empty working copy, and warm through a node daemon (`bee-daemon`) whose
   disk cache already holds the objects.
8. `git clone` over smart HTTP (protocol v2), three clones.
9. Code search (`SearchService.SearchCode`) latency once the index has caught up.

Availability is a monthly SLO; a run reports the observed success ratio of its own requests.

## Results

Two rounds: the W14 run of 2026-10-19 that found the problems ("before"), and the re-run of
2026-10-20 after the fixes listed under "What changed" ("after"). Same machine, same harness
and workload; the full "after" reports follow the summary.

### Summary: before and after

| SPEC §16 target | SQLite before | SQLite after | Postgres before | Postgres after |
|---|---|---|---|---|
| CreateChangeset p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size | **miss**: 18.0 / 31.0 ms (11,400 files) | **miss** on 2026-10-20: 20.0 / 41.5 ms (10.8 ms at 400 files). **Hit** on 2026-11-15 after the fix: 7.15 / 23.2 ms (7.78 ms at 400 files) | hit, no margin: 9.96 / 12.4 ms | **miss** on 2026-10-20: 12.9 / 18.3 ms (3.3 ms at 400 files). **Hit** on 2026-11-15 after the fix: 3.07 / 5.12 ms (4.63 ms at 400 files) |
| Land, uncontended, p50 ≤ 30 ms, p99 ≤ 150 ms | hit: 7.0 / 15.0 ms | hit: 6.9 / 30.2 ms | hit: 2.1 / 4.2 ms | hit: 3.1 / 5.8 ms |
| Hot ref ≥ 100 lands/s, p99 time-to-land ≤ 10 s, 1,000 landers | **miss**: 73 lands/s, p99 13.6 s | hit: 250 lands/s, p99 4.0 s | hit: 268 lands/s, p99 3.7 s (forge in SQLite) | hit: 171 lands/s, p99 5.8 s (forge in the same Postgres) |
| Sparse checkout 1,000 files, warm ≤ 1 s | **miss**: 4.33 s | hit: 0.38 s | **miss**: 4.13 s | hit: 0.36 s |
| Sparse checkout 1,000 files, cold ≤ 3 s | **miss**: 4.44 s | hit: 0.74 s | **miss**: 4.43 s | hit: 0.71 s |
| Log / blame / diff p99 ≤ 200 ms | hit: 19.6 / 8.4 / 3.0 ms | hit: 45.6 / 34.6 / 4.8 ms | hit: 18.2 / 11.8 / 3.1 ms | hit: 19.5 / 9.3 / 3.1 ms |
| Event delivery p99 ≤ 1 s (10,000 subscribers) | hit: 434 ms | hit: 655 ms | hit: 475 ms | hit: 663 ms |
| ≥ 10,000 sessions with streams open | met exactly | met exactly | met exactly | met exactly |
| Forge event log after the 1,000-lander burst (no SPEC bound) | stuck at main v216 of 1,208 for 17 min | caught up in 44 ms | stuck at v216 of 1,208 | caught up in 23 ms |
| Forge on Postgres (default) | – | – | failed: 49,008 connects in 3.5 min, loopback ports exhausted | works (the "after" run) |
| Availability (monthly SLO) | 100 % of 2,645 requests | 100 % of 2,645 | 100 % of 2,645 | 100 % of 2,645 |
| Load average, 1 min (before → after the run) | 3.6 → 5.6 | 5.2 → 12.7 | 3.1 → 3.9 | 5.7 → 6.9 |

In the 2026-10-20 runs everything SPEC §16 bounds was met on both stores except
**CreateChangeset in an 11,400-file repo**. That is fixed and re-measured in
"CreateChangeset at repo size (2026-11-15)" below. The 2026-11-15 runs found **event
delivery** missing its p99 ≤ 1 s bound on both stores, with the code before and after that
fix alike (see that section); it is open.

### What changed between the runs

1. **Forge event log stall.** The VCS→forge bridge appends a batch of up to 500 events in one
   transaction that also advances the repo's cursor; listeners of batched appends get the
   events through a bounded queue each, drained by their own task, so a slow listener never
   holds the log back; the security dependency-graph listener coalesces moves per ref into one
   worker and works from the paths changed since its stored graph (a tree diff), listing a
   tree only for a repo's first graph. Regression tests:
   `crates/bee-conformance/tests/forge_events.rs` (a burst of 200 lands on a 3,000-file tree
   must reach the log within 10 s: 195 of 202 after 21.6 s before, 70 ms after) and bridge and
   security tests in `bee-forge`.
2. **Forge Postgres pool.** A transaction dropped without commit is rolled back in the
   background and its connection returned to the pool. Test (under `BEE_TEST_PG`):
   320 uncommitted transactions on a 3-connection pool open 320 connections before, ≤ 3 after.
3. **Stale reads during a metadata outage** (SPEC §15.1): `ResolveRef` answers the last version
   this process read, marked `stale`, within `metadata.stale_read_max_ms` (default 5 min) and
   never below the client's `min_version`. The fault test now asserts the stale answer.
4. **Coalescer owner identity.** Forwarded lands carry the owner's instance id and fence; any
   other process refuses them before authenticating, the forwarder lands itself and takes the
   lease over. The tests no longer need the `SERIAL` lock. Two latent failover bugs surfaced
   on the way and were fixed: a write cancelled mid-transaction left SQLite's write lock held
   by a crashed process's store, and a stopped server could start a fresh coalescer owner for
   a land still in flight.
5. **Sparse checkout.** `bee-wc` no longer issues a full sync (an `F_FULLFSYNC` of a few
   milliseconds on macOS) per written file: an apply pushes all its files and directories to
   the device with plain `fsync`s over a few threads and then flushes the device cache once,
   before the journal records the apply. 501 full syncs for 500 files before, 1 after.
6. **SQLite hot ref.** A coalesced batch rebases over a buffered history
   (`bee_store::StagedMeta`) and writes every rebased stack's trees and changeset in one
   `MetaStore::put_batch` transaction before its CAS: 80 write transactions for 40 stacks in
   2 batches before, 2 after (`complexity.rs::a_coalesced_batch_commits_once_not_twice_per_stack`).

### SQLite (dev cell default), after

- Date: 2026-10-20 18:03 UTC
- Machine: Apple M4 Pro, 14 cores, 24 GiB, macos
- Store: SQLite (WAL, synchronous = FULL, F_FULLFSYNC on macOS); blobs on local disk
- Build: release
- Load average (1 min): 5.15 before, 12.70 after
- Workload: 200 samples per latency row, 1000 landers, 10000 subscriptions, repo grown by 10000 files
- One in-process server, gRPC over loopback; latencies are client-observed

| Operation | Target | p50 | p99 | Measured | Result | Notes |
|---|---|---|---|---|---|---|
| CreateChangeset, 1 file changed, server time | p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size | 20.00 ms | 41.49 ms | 200 samples | **miss** (by 100 %) | repo of 11400 files; in a 400-file repo: p50 10.75 ms, p99 19.07 ms |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms, p99 ≤ 150 ms | 6.91 ms | 30.18 ms | 200 samples | hit (77 % margin) |  |
| Land throughput, one hot ref, 1,000 concurrent landers | ≥ 100 changes/s, every lander lands within 10 s p99 | 2913.58 ms | 3986.78 ms | 250 lands/s; 1000 landers | hit (60 % margin) | 4 CAS batches (avg 250.0 lands per batch); wall 3.99 s; max time-to-land 3990.05 ms |
| Sparse checkout, 1,000 files, warm node cache | ≤ 1 s | 380.35 ms | 380.35 ms | 1 samples | hit (62 % margin) | bee-wc clone through a node daemon with a warm disk cache |
| Sparse checkout, 1,000 files, cold, same region | ≤ 3 s | 742.50 ms | 742.50 ms | 1 samples | hit (75 % margin) | bee-wc clone straight from the server (loopback), empty working copy |
| Log via API, 100-entry page | p99 ≤ 200 ms | 17.30 ms | 45.56 ms | 100 samples | hit (77 % margin) | under 32 mixed readers: p50 341.86 ms, p99 365.37 ms |
| Blame via API, one file of 10k lines | p99 ≤ 200 ms | 6.42 ms | 34.60 ms | 100 samples | hit (83 % margin) | 10,000 lines, 50 edits; under 32 mixed readers: p50 117.30 ms, p99 151.58 ms |
| Diff via API, one file of 10k lines | p99 ≤ 200 ms | 2.49 ms | 4.75 ms | 100 samples | hit (98 % margin) | 10,000-line file with hunks; under 32 mixed readers: p50 10.82 ms, p99 17.47 ms |
| Event delivery, commit to subscriber | p99 ≤ 1 s | 365.36 ms | 655.24 ms | 10000 streams open | hit (34 % margin) | 50000 deliveries of 5 lands to 10000 subscribers; 10000 subscribers got all |
| Concurrent agent sessions with streams open | ≥ 10,000 per cell | – | – | 10000 streams open | hit (0 % margin) | 10000 subscriptions attempted over 100 connections by 100 agent tokens; 10000 opened, 0 refused; counted: those that received every event |
| Availability | reads 99.99%, writes 99.95%, per cell, per month | – | – | observed reads 100.0000 %, writes 100.0000 % over 2645 requests | not judged: a monthly SLO; one run reports its observed success ratios | every harness request |

| Extra measurement | Value |
|---|---|
| Forge event log after the 1000-lander burst (polled every 200 ms) | burst's last land in the log after 43.88 ms; a land made right after it after 43.88 ms; log held main up to version 1208 of 1208 |
| Mixed reads (log, blame, diff, list tree, resolve), 32 concurrent readers | 332 reads/s |
| git clone over smart HTTP (12650 files, 16.7 MiB packed) | p50 1611.41 ms (10.3 MiB/s, 6826 files/s) |
| Code search, literal queries over 11000 files (no SPEC §16 bound) | p50 1.40 ms, p99 3.39 ms (100 queries); index caught up in 0.2 s |

An earlier SQLite run the same evening, at load 11.3 → 6.9, measured the same picture except
event delivery (p99 1,451 ms, a miss) and CreateChangeset (p50 29.3 ms, p99 206.7 ms); hot ref
275 lands/s, sparse checkout 0.29 s warm and 0.58 s cold, forge log caught up in 29 ms. It is
listed so the delivery row is not read as settled: at 10,000 subscribers it is load-sensitive.

### Postgres, after (forge tables in the same Postgres, the default)

- Date: 2026-10-20 17:57 UTC
- Machine: Apple M4 Pro, 14 cores, 24 GiB, macos
- Store: Postgres 17 (local primary + streaming standby, fsync = on, synchronous_commit = on); forge tables in the same Postgres; blobs on local disk
- Build: release
- Load average (1 min): 5.69 before, 6.89 after
- Workload: 200 samples per latency row, 1000 landers, 10000 subscriptions, repo grown by 10000 files
- One in-process server, gRPC over loopback; latencies are client-observed

| Operation | Target | p50 | p99 | Measured | Result | Notes |
|---|---|---|---|---|---|---|
| CreateChangeset, 1 file changed, server time | p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size | 12.88 ms | 18.34 ms | 200 samples | **miss** (by 29 %) | repo of 11400 files; in a 400-file repo: p50 3.25 ms, p99 5.38 ms |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms, p99 ≤ 150 ms | 3.10 ms | 5.76 ms | 200 samples | hit (90 % margin) |  |
| Land throughput, one hot ref, 1,000 concurrent landers | ≥ 100 changes/s, every lander lands within 10 s p99 | 2768.96 ms | 5837.28 ms | 171 lands/s; 1000 landers | hit (42 % margin) | 3 CAS batches (avg 333.3 lands per batch); wall 5.85 s; max time-to-land 5842.07 ms |
| Sparse checkout, 1,000 files, warm node cache | ≤ 1 s | 360.04 ms | 360.04 ms | 1 samples | hit (64 % margin) | bee-wc clone through a node daemon with a warm disk cache |
| Sparse checkout, 1,000 files, cold, same region | ≤ 3 s | 705.70 ms | 705.70 ms | 1 samples | hit (76 % margin) | bee-wc clone straight from the server (loopback), empty working copy |
| Log via API, 100-entry page | p99 ≤ 200 ms | 14.24 ms | 19.54 ms | 100 samples | hit (90 % margin) | under 32 mixed readers: p50 103.14 ms, p99 111.61 ms |
| Blame via API, one file of 10k lines | p99 ≤ 200 ms | 5.70 ms | 9.26 ms | 100 samples | hit (95 % margin) | 10,000 lines, 50 edits; under 32 mixed readers: p50 39.48 ms, p99 47.92 ms |
| Diff via API, one file of 10k lines | p99 ≤ 200 ms | 2.44 ms | 3.05 ms | 100 samples | hit (98 % margin) | 10,000-line file with hunks; under 32 mixed readers: p50 6.67 ms, p99 9.96 ms |
| Event delivery, commit to subscriber | p99 ≤ 1 s | 375.93 ms | 663.43 ms | 10000 streams open | hit (34 % margin) | 50000 deliveries of 5 lands to 10000 subscribers; 10000 subscribers got all |
| Concurrent agent sessions with streams open | ≥ 10,000 per cell | – | – | 10000 streams open | hit (0 % margin) | 10000 subscriptions attempted over 100 connections by 100 agent tokens; 10000 opened, 0 refused; counted: those that received every event |
| Availability | reads 99.99%, writes 99.95%, per cell, per month | – | – | observed reads 100.0000 %, writes 100.0000 % over 2645 requests | not judged: a monthly SLO; one run reports its observed success ratios | every harness request |

| Extra measurement | Value |
|---|---|
| Forge event log after the 1000-lander burst (polled every 200 ms) | burst's last land in the log after 23.02 ms; a land made right after it after 23.02 ms; log held main up to version 1208 of 1208 |
| Mixed reads (log, blame, diff, list tree, resolve), 32 concurrent readers | 1009 reads/s |
| git clone over smart HTTP (12650 files, 16.7 MiB packed) | p50 1662.62 ms (10.0 MiB/s, 6616 files/s) |
| Code search, literal queries over 11000 files (no SPEC §16 bound) | p50 4.80 ms, p99 7.98 ms (100 queries); index caught up in 0.5 s |

### Reading the numbers

- **Load.** The "after" runs started at a 1-minute load average of 5.2 (SQLite) and 5.7
  (Postgres), above the "before" runs' 3.6 and 3.1: other agents' builds shared the machine.
  Latency rows are therefore slightly pessimistic against "before"; the counted fixes
  (commits per batch, full syncs per checkout) do not depend on load.
- **CreateChangeset vs repo size** was the one target still missed on both stores in these
  runs. The row counts were flat, which pointed away from the store, but the cause was in it
  after all: rows of very different sizes. Fixed on 2026-11-15; see the next section.
- **Hot ref.** SQLite now forms 4 batches of 250 stacks, each costing one flush and one CAS
  instead of about 500 commits. Postgres reached 171 lands/s with the forge's tables in the
  same database (the default; the "before" Postgres number, 268/s, had the forge in SQLite),
  comfortably over the target.
- **Sparse checkout** is now well inside both targets; warm through the daemon is about half
  the cold time.
- **Event delivery** stays within its bound on both stores at 10,000 subscribers, with less
  margin than before and one load-sensitive miss in an earlier SQLite run (see above). Every
  open subscription still re-reads the forge event store when the log moves (now once per
  bridged batch rather than once per event); a shared fan-out of each new page to all
  subscribers of a log is the remaining improvement, listed for G1.

## CreateChangeset at repo size (2026-11-15)

SPEC §16 asks for CreateChangeset p50 ≤ 10 ms and p99 ≤ 50 ms independent of repo size. The
2026-10-20 runs missed it in the 11,400-file repo on both stores (SQLite 20.0 ms p50, 10.8 ms
at 400 files; Postgres 12.9 ms, 3.3 ms at 400 files).

### Cause

A per-step timing probe of `create_impl` (temporary instrumentation, not committed; release
build, 200 samples per row, SQLite, load average 22.5 at the start) located the time:

| Step (p50) | 400 files, one-file parent | 11,400 files, parent = the bulk changeset |
|---|---|---|
| Build: parent root, path trees, new trees | 0.19 ms | 9.06 ms |
| Upload of the meta and new trees (its own commit) | 4.78 ms | 4.74 ms |
| Changeset row with edges, `object_paths`, eager rows (its own commit) | 4.93 ms | 4.95 ms |
| Everything else (auth, push rules, push protection, eager rows, audit, view) | 2.05 ms | 1.13 ms |
| Client-observed | 13.00 ms | 20.25 ms |

1. **Parent size, not repo size.** The harness grows the repo with one 11,000-file
   changeset and then commits on top of that changeset itself (as an agent does right after
   an import). `Writer::build` read and decoded the first parent's whole row (its 11,000-entry
   delta, about 900 KB) only to learn its root tree. The counted rows were flat because it is
   one row either way. On a one-file parent in the same 11,400-file repo the build took
   0.44 ms.
2. **Two durable commits per call.** The meta and new trees were uploaded in one transaction
   and the changeset row written in a second, each a full sync (`F_FULLFSYNC` on macOS,
   about 5 ms). That was the floor at any size.

### Fix

- `Changeset::root_from_prefix` (`bee-object`) reads the root from the leading bytes of a
  canonical encoding: the format version, at most 16 parents and the root take at most 618
  bytes. `MetaStore::get_changeset_root` uses it; the SQL stores select
  `substr(bytes, 1, 1024)` and decode nothing past the root.
  `Writer::build` reads the parent's root this way. Ids, encodings and frozen vectors are
  unchanged; the read is a pure function of the stored bytes.
- `MetaStore::insert_changeset_with_objects` writes the meta and new trees' object rows (with
  their upload record) and the changeset row in one transaction on the SQL stores; the
  default for other stores makes the two calls in order. `Writer::store_new` is the
  CreateChangeset entry point.
- **Durability (SPEC §6.2, amended):** blob-store bytes of non-inline objects are still
  written and durable before the transaction; the object rows commit together with the
  changeset row that refers to them, so no reader can see the changeset without its objects,
  or an object row without durable bytes. A crash before the commit leaves nothing but orphan
  blob-store bytes, which GC removes. A missing referenced object rolls the whole transaction
  back (`bee-store-sql` test `a_new_changeset_commits_once_and_parents_are_read_by_root`,
  on SQLite and Postgres). Invariant D is unaffected: the trees are built exactly as before.

### Load harness, before and after

The full harness command from "How to run", release builds of the code before the fix
(`cfd268a6`, with the probe's timing calls compiled in) and after it, run back to back on
the same machine (Apple M4 Pro, 14 cores, 24 GiB, macOS 27.0.1) while holding the shared
build lock so no build overlapped. Other agents' processes still shared the machine.

| Store | Code | Load (1 min) start → end | 11,400 files p50 / p99 | 400 files p50 / p99 | Result |
|---|---|---|---|---|---|
| SQLite | before | 7.42 → 8.40 | 19.79 / 527.13 ms | 12.55 / 20.21 ms | **miss** (by 954 %) |
| SQLite | after | 8.28 → 8.13 | 7.15 / 23.22 ms | 7.78 / 14.84 ms | hit (28 % margin) |
| Postgres | before | 6.83 → 11.52 | 11.45 / 18.08 ms | 3.60 / 6.44 ms | **miss** (by 14 %) |
| Postgres | after | 15.95 → 10.10 | 3.07 / 5.12 ms | 4.63 / 13.12 ms | hit (69 % margin) |

Load averages are the ones each run's report records. Every other SPEC §16 row of the four
runs agrees between before and after: uncontended land 6.19 / 6.26 ms p50 (SQLite) and
3.41 / 3.41 ms (Postgres); 1,000 landers on one ref in 4.43 / 3.74 s (SQLite) and
4.53 / 4.35 s (Postgres); sparse checkout, log, blame and diff hit in all four runs.

**Event delivery misses on both stores in all four runs**, before the fix as well: p99
3,613.93 / 4,056.67 ms on SQLite and 11,667.45 / 5,479.93 ms on Postgres, against ≤ 1 s
(655 and 663 ms on 2026-10-20 at load 5.2 and 5.7). This change does not touch the event
path; the cause is open (load is one candidate: the 2026-10-20 note already called this row
load-sensitive, and an earlier run that day measured 1,451 ms).

### Counted gate

`crates/bee-conformance/tests/complexity.rs::create_changeset_work_is_independent_of_repo_and_parent_size`
builds two repos with the same history (a bulk changeset landed, then a one-file land), of
400 and 11,400 files, and creates the same one-file change at depth 2 on the bulk changeset
and on the one-file land. Counted (SQLite, debug build):

| Parent | Repo | Transactions | Rows read | Rows written | Tree fetches | Blob fetches | Durable commits | Changeset bytes read |
|---|---|---|---|---|---|---|---|---|
| bulk changeset | 400 files (399 entries) | 18 | 29 | 16 | 1 | 1 | 1 | 1,217 |
| bulk changeset | 11,400 files (11,399 entries) | 18 | 29 | 16 | 1 | 1 | 1 | 1,217 |
| one-file land | 400 files | 18 | 29 | 16 | 1 | 1 | 1 | 379 |
| one-file land | 11,400 files | 18 | 29 | 16 | 1 | 1 | 1 | 379 |

The gate asserts identical work at both sizes for each parent, one durable commit, at most
1.5 KiB of changeset rows read (the parent's 1 KiB prefix and the new row), one blob read
and at most three tree fetches. `changeset_bytes_read` is a new access counter: row counts
alone could not see this bug. The commit-graph skip chain still costs O(log depth) graph-row
reads in history length, as designed; the gate compares parents at equal depth.

## bee vs git

The SPEC §16 harness judges bee against its own targets. This section puts it next to
plain git on the same machine, the same content and the same agent workloads.

```sh
cargo build --release -p bee-conformance --example vs_git
target/release/examples/vs_git --agents 10,50,100 --files 10000 --samples 50 --clones 3 \
    --out report.md
```

The harness (`crates/bee-conformance/src/load/vs_git.rs`) starts an in-process
bee-server and a plain git server side by side, seeds both with the same 1,000 files,
and runs:

1. **Contention.** N agents each commit one new file on the same stale tip of `main`, then
   race to land it.
   - A git agent pushes. When the push is rejected, it pulls with `--rebase` and pushes
     again until it lands.
   - Each agent is timed from the starting barrier until its change is on `main`.
2. **Growth.** Both repos then grow by 10,000 files, a 1,000-file `sparse/` directory, and a
   10,000-line `big.txt` edited 50 times (one commit per edit).
3. **Full clone and sparse checkout**, three runs each.
4. **History reads**, 50 samples each.

### Run of 2026-10-26, before the fixes

- Machine: Apple M4 Pro, 14 cores, 24 GiB, macOS. It was shared with other jobs; the 1-minute
  load average was 10.0 before the run and 21.4 after.
- git 2.43.0. The git server runs `git http-backend` per request behind a small CGI shim
  (smart HTTP, protocol v2, loopback). The git client uses `http.postBuffer = 512 MiB`, so
  large pushes carry a length rather than a chunked body.
- bee: release build, SQLite (WAL, `synchronous = FULL`, `F_FULLFSYNC` on macOS), disk
  blobs.
- One run per row. Treat differences under about 2× as noise unless they repeat.

**Contention: N agents land one commit each on `main`**

| Agents | System | Landed | Wall | Lands/s | Time to land p50 | p99 | Push or land attempts |
|---|---|---|---|---|---|---|---|
| 10 | git server, default config | 10/10 | 3.49 s | 2.9 | 2.00 s | 3.49 s | 55 |
| 10 | git server, `core.fsync=all` | 10/10 | 5.32 s | 1.9 | 3.31 s | 5.32 s | 55 |
| 10 | bee git gateway, stock git client | 10/10 | 0.29 s | 34.7 | 0.26 s | 0.29 s | 10 |
| 10 | bee API | 10/10 | 0.23 s | 43.1 | 0.22 s | 0.23 s | 10 |
| 50 | git server, default config | 50/50 | 72.68 s | 0.7 | 53.96 s | 72.68 s | 1,273 |
| 50 | git server, `core.fsync=all` | 50/50 | 48.45 s | 1.0 | 32.83 s | 48.45 s | 1,166 |
| 50 | bee git gateway, stock git client | 50/50 | 1.53 s | 32.8 | 1.34 s | 1.53 s | 50 |
| 50 | bee API | 50/50 | 1.15 s | 43.4 | 1.11 s | 1.15 s | 50 |
| 100 | git server, default config | 100/100 | 237.79 s | 0.4 | 163.08 s | 237.45 s | 4,081 |
| 100 | git server, `core.fsync=all` | 100/100 | 202.44 s | 0.5 | 147.20 s | 202.12 s | 4,106 |
| 100 | bee git gateway, stock git client | 100/100 | 6.37 s | 15.7 | 5.82 s | 6.36 s | 100 |
| 100 | bee API | 100/100 | 5.21 s | 19.2 | 5.00 s | 5.20 s | 100 |

**Full clone (12,321 files)**

| Client ← server | p50 | min | max | Files written |
|---|---|---|---|---|
| `git clone` ← git server | 1.82 s | 1.69 s | 1.91 s | 12,321 |
| `git clone` ← bee git gateway | 1.82 s | 1.50 s | 8.68 s | 12,321 |
| bee working copy ← bee server | 7.44 s | 6.56 s | 11.37 s | 12,301 |

**Sparse checkout of the 1,000-file `sparse/` directory**

| Client ← server | p50 | min | max |
|---|---|---|---|
| `git clone --filter=blob:none` + `sparse-checkout` ← git server | 386 ms | 386 ms | 523 ms |
| `git clone --filter=blob:none` + `sparse-checkout` ← bee git gateway | 212 ms | 203 ms | 1.87 s |
| bee sparse working copy ← bee server | 501 ms | 454 ms | 551 ms |

**History reads on `big.txt` (10,000 lines, 50 edits)**

| Read | git, local full clone, p50 / p99 | bee API over loopback gRPC, p50 / p99 |
|---|---|---|
| Log, 100 entries | 6.4 / 13.4 ms | 31.8 / 41.5 ms |
| Blame | 33.8 / 41.8 ms | 5.3 / 406.6 ms |
| Diff of the last commit | 7.9 / 14.7 ms | 2.8 / 10.1 ms |

### Run of 2026-10-26 22:30 UTC, after the fixes in Spaces #197

Same machine, harness and flags as above. Two full runs (a, b), both on the final code,
after a green `scripts/check.sh`. The 1-minute load average was 6.8 to 12.2. Values are
p50 unless marked; "a / b" gives each run.

**Before and after**

| Row | git (a / b) | bee before (one run) | bee after (a / b) | Result |
|---|---|---|---|---|
| 100 agents land on `main`, bee API, wall | 99.8 s / 97.5 s | 5.21 s | 2.43 s / 2.52 s | bee, about 40× |
| 100 agents, stock git client through the gateway, wall | 99.8 s / 97.5 s | 6.37 s | 2.96 s / 3.02 s | bee, about 33× |
| Full clone, `git clone` through the gateway | 1.44 s / 1.45 s | 1.82 s | 1.13 s / 1.14 s | bee |
| Full clone through the gateway, first (cold) clone | 1.46 s / 1.47 s (max) | 8.68 s | 3.60 s / 3.63 s | **git** |
| Full checkout, bee working copy | 1.44 s / 1.45 s | 7.44 s, 20 files missing | 3.84 s / 4.04 s, all 12,321 files | **git, about 2.7×** |
| Sparse 1,000 files, bee working copy | 328 ms / 327 ms | 501 ms | 283 ms / 278 ms | bee |
| Sparse 1,000 files, partial clone through the gateway | 328 ms / 327 ms | 212 ms | 167 ms / 179 ms | bee |
| Log, 100 entries, p50 / p99 | 4.61 / 6.47 ms, 4.62 / 6.32 ms | 31.8 / 41.5 ms | 4.09 / 4.93 ms, 4.17 / 4.96 ms | bee, narrowly |
| Blame, p50 / p99 | 29.9 / 33.0 ms, 30.1 / 33.4 ms | 5.3 / 406.6 ms | 1.65 / 2.72 ms, 1.69 / 2.89 ms | bee |
| Diff of the last commit, p50 | 4.87 ms / 5.24 ms | 2.8 ms | 2.00 ms / 2.07 ms | bee |

git's contention numbers this time were lower than the first run's (97 to 100 s at 100
agents against 237 s); the machine was less loaded. The full tables of each run are in
`target/vs_git/report4a.md` and `report4b.md` when regenerated locally.

**What changed (Spaces #197)**

- **Working copy missing files (fixed).** `History::list_tree` dropped a directory's whole
  subtree when a page ended exactly on that directory. Every client now writes 12,321 files.
  Test: `read_path_and_paginated_list_tree` pages at every page size.
- **Working-copy checkout.** `GetObjects` batches run up to 8 at a time
  (`Transport::server_streams`; the CLI transport still sends them one after another). Files
  are created in place when nothing is at the name (temp file and rename only to replace),
  with the root and parent directory handles kept open between files and the stat taken
  from the open file. Files are still flushed to disk before the journal records the apply.
- **Log.** Most of the time was policy, not storage: the page includes the 11,000-file bulk
  commit, and every path was checked with a full `path_allowed`. `Policy::access_filter` and
  `Scope::allows_path` now do the path-independent work once per request (tests prove they
  agree with `check_access_chain` and `Scope::check`). Changesets and graph nodes are shared
  between requests on a repo (`server.share_history_rows`), and a page's metas are read in
  one batch.
- **Blame.** After each land, the server derives the blame of the files it put, in the
  background (`land.warm_blame`), so the first request reads a stored value.
- **Gateway clone.** After each land, the server derives the git ids of the new tip in the
  background (`land.warm_git`), one task per repo, merging lands that queue while it runs.

### Follow-up fixes (Spaces #200–#203), 2026-10-27

What changed after the run above:
- **Gateway clone:** the server builds the stock clone pack right after a land (`land.warm_pack`; #202).
- **Working-copy checkout (#201):**
  - The working copy fetches `GetObjects` and `ListTree` in binary protobuf, with 8 streams in flight.
  - It overlaps fetching and writing in groups of 2,048 files.
  - On the server, `GetObjects` reads redactions and object rows once per request.
- **CLI:** the bee CLI fetches the same way when it talks to the server directly (#203).
- **Combinations:** changes that pass alone but fail together are kept out of deploys (#200). This does not affect these timings.

**Quick run.** One quick run on that code: one contention agent, 50 samples, three clones per row. The 1-minute load average was 13.3 before and 6.5 after. Values are p50 unless marked.

| Row | git | bee, run above | bee now |
|---|---|---|---|
| `git clone` through the gateway, warm | 1.50 s | 1.13 s | 1.23 s |
| First (cold) `git clone` after a land, slowest of three | 1.54 s (git's slowest) | 3.60 s | 1.33 s |
| Full checkout, bee working copy | 1.50 s | 3.84 s | 3.14 s |
| Sparse 1,000 files, bee working copy | 297 ms | 283 ms | 218 ms |
| Sparse 1,000 files, partial clone through the gateway | 297 ms | 167 ms | 199 ms |
| Log, 100 entries, p50 / p99 | 6.0 / 8.2 ms | 4.1 / 4.9 ms | 5.0 / 5.5 ms |
| Blame, p50 / p99 | 33.6 / 40.5 ms | 1.7 / 2.7 ms | 1.5 / 3.2 ms |

**Reading it.**
- The first clone after a land no longer loses. Its slowest case is now inside git's range, where it was 2.5× slower before.
- The full working-copy checkout is still about 2× slower than `git clone`. One probe on this code, with timing instrumentation, split its 2.69 s as follows:

  | Stage | Time |
  |---|---|
  | Manifest | 0.48 s |
  | Planning | 0.13 s |
  | Fetch and write, overlapped (fetch-bound) | 1.77 s |
  | Sync | 0.30 s |

  Sampling during the fetch showed filesystem work on both sides. The server opens about 12,000 separate blob files, one file per blob in the disk store, where a git server reads one packfile. The client creates about 10,000 directories for this layout. Closing the rest needs small blobs stored together (inline or packed), which is a storage-format change.

**Full runs discarded.** Two full runs on this code (10, 50 and 100 agents) were taken while other builds shared the machine: the 1-minute load average was 40 and then up to 76. git's own full clone ranged from 1.5 s to 7.2 s across them, so their absolute times are not reported. The relative picture in them matches the table: contention, gateway clones, sparse checkouts and reads still favour bee, and the full working-copy checkout still loses. A clean full run should be repeated on an idle machine.

### What is being compared

- **bee API timings include the whole write.** Each agent's upload, `CreateChangeset`
  and `Land` all fall inside the timed window. A git agent's local commit was made before
  the barrier, so git's numbers count only push, pull and rebase.
- **Durability.**
  - bee flushes every land with `F_FULLFSYNC`.
  - git's defaults do not flush ref updates. With `core.fsync=all` and
    `core.fsyncMethod=fsync`, git 2.43 issues `fcntl(F_FULLFSYNC)` for a hardware flush on
    macOS (`git_fsync` in `wrapper.c`).
  - Both git configurations are reported.
- **git reads are local.** They run in a full clone with no network, so their cost includes
  having cloned first (1.82 s here). bee reads go over gRPC on loopback and need no clone.
- **The git server is a stand-in.** It spawns one `git http-backend` per request, which is
  how stock git serves smart HTTP, not how a production forge does.

### Reading the numbers

- **Contention is the clearest difference** (numbers from the first run; the second agrees).
  - A git push on a moved tip is rejected, so each round lands one agent and the rest
    rebase and retry. At 10 agents that took exactly 55 pushes (10 + 9 + … + 1).
  - git's throughput fell from 2.9 to 0.7 to 0.4 lands per second as agents went from 10 to
    50 to 100.
  - bee lands every agent on its first attempt. The coalescer rebases server-side, and
    that also holds for a stock git client pushing through the gateway (`docs/spec/git-gateway.md`, protected refs).
  - At 100 agents, the last change landed after 237 s on git and 5.2 s on the bee API,
    about 46× sooner. Throughput was 0.42 against 19.2 lands per second.
- **`core.fsync=all` was faster than the default at 50 and 100 agents.** That is variance in
  the retry race on a loaded machine, not an effect of flushing. Its own 10-agent row is
  slower, as expected.
- **These bee lands per second are below the SPEC §16 hot-ref row (250/s).** Our
  inference, not something we measured: each timed window here includes the agent's own
  upload and `CreateChangeset`, and 10 to 100 landers fill the coalescer's batches far less
  than 1,000 do.
- **Clone.**
  - Through the gateway, a warm `git clone` is now faster than the git server (1.13 s against
    1.44 s).
  - **The first clone after a large land is still slower than git** (3.6 s against 1.5 s). The
    git ids are derived ahead of time now; what remains is building the pack itself, which
    the pack cache keeps only once a client has asked for it.
  - **The bee working copy is still about 2.7× slower than `git clone` for a full
    checkout** (3.84 s against 1.44 s). Before the fixes, a probe split the time into the
    manifest listing (0.4 s), fetching blobs as JSON (2.4 s, now 1.15 s), writing 12,321 files
    into 10,000 one-file directories, and flushing them. Binary fetches and a cheaper
    manifest are the next steps.
- **Sparse checkout.** Both bee paths now beat the git server: a blobless partial clone
  through the gateway (167 ms) and the bee working copy (283 ms), against 328 ms.
- **Reads.**
  - The log over loopback gRPC is now level with a local `git log` at the median (4.1 ms
    against 4.6 ms) and steadier at p99 (4.9 ms against 6.4 ms). Most of what remains is
    encoding the 11,000-entry delta of the bulk commit, which the response carries.
  - Blame is about 18× faster than git (1.7 ms against 30 ms), and its p99 is now 2.9 ms:
    the value is derived when the change lands, not on the first request.

## Complexity gates (SPEC §14.6)

`crates/bee-conformance/tests/complexity.rs` grows one repository in three stages, each 10×
the previous in history and in files, and counts the storage work of each operation with the
metadata and blob stores' access counters (never wall-clock time, so the gate holds on a loaded
machine). The forge, the search indexer and the ops sampler are off in this test so that
background readers do not add to the counters. Counted work at 1× / 10× / 100× (5 / 50 / 500
changesets and 100 / 1,000 / 10,000 files; the gate now runs 2 / 20 / 200 changesets and 50 / 500 / 5,000 files to stay inside the
test-time budget, `BEE_SCALE_FULL=1` runs 20 / 200 / 2,000 and up to 100,000 files):

| Operation | Transactions | Rows read | Rows written | Tree fetches | Blob reads | Gate |
|---|---|---|---|---|---|---|
| One-file CreateChangeset (re-counted 2026-11-15) | 20 / 25 / 27 | 31 / 36 / 38 | 16 / 15 / 15 | 1 / 2 / 2 | 1 / 1 / 1 | sub-linear; reads only its own new blob (push protection); one durable commit, see `create_changeset_work_is_independent_of_repo_and_parent_size` |
| Land, uncontended | 14 / 14 / 14 | 18 / 18 / 18 | 5 / 5 / 5 | 1 / 1 / 1 | 0 | exactly constant |
| Land over an unrelated tip (rebase; re-counted 2026-11-15) | 39 / 47 / 53 | 52 / 60 / 66 | 16 / 16 / 16 | 8 / 9 / 9 | 0 | sub-linear; zero blob reads |
| Log, 10-entry page | 44 / 44 / 44 | 44 / 44 / 44 | 0 | 0 | 0 | exactly constant |
| Log filtered to one file | 30 / 30 / 30 | 30 / 30 / 30 | 0 | 0 | 0 | exactly constant |
| Blame, file with 5 edits | 63 / 22 / 22 | 58 / 22 / 22 | 5 / 0 / 0 | 0 | 9 / 0 / 0 | constant after the first derivation |
| Diff with classification | 23 / 25 / 25 | 22 / 24 / 24 | 1 / 1 / 1 | 6 / 6 / 6 | 2 / 4 / 4 | constant after the first stage |
| Sparse fetch of 50 files | 208 / 208 / 208 | 208 / 208 / 208 | 0 | 2 / 2 / 2 | 50 / 50 / 50 | exactly constant |

The test asserts: no counter more than doubles (+2) across the 100× growth; land, both log
pages and the sparse fetch are identical at every size; blame and classified diff are identical
once derived; a rebase over an unrelated tip reads zero blobs; a one-file commit reads only its
own blob and at most three trees. The slow growth of CreateChangeset and of the rebasing land
(+7 and +14 single-row transactions over 100× in the 2026-11-15 count) is logarithmic-looking,
not linear. Part of it is the commit graph's skip chain, O(log depth) graph-row reads by
design: a traced run of the new CreateChangeset gate showed one more graph read on a parent
one commit deeper and nothing else different. Status against tree size is a working-copy property with
no server storage involved; its gate is `bee-daemon`'s
`watcher_status_rescans_only_changed_paths_on_a_large_tree`.

## Correctness at scale

| Suite | What it proves | Gate size | Full size (`BEE_SCALE_FULL=1`) |
|---|---|---|---|
| `tests/linearizability.rs` | ResolveRef / UpdateRef / Land histories are linearizable against the reference ref model (Wing–Gong checker with memoization, `bee_conformance::load::history`): with no faults; with injected store faults (errors, "committed, response lost", delays); and across coalescer owner failover (the owner crashes mid-batch, another process lands, a third joins) with the fencing token checked. Every land result must sit on the tip it landed on, so stacks land "as if alone at that position". Seeds are printed; `BEE_SCALE_SEED` replays. | 8 clients × 30 ops | 32 × 150 |
| `tests/forge_events.rs` | A burst of concurrent lands on a large tree (with a manifest, so the dependency-graph listener has work) reaches the forge event log within 10 s, and a land right after it within 5 s. | 200 landers, 3,000 files | 1,000, 10,000 |
| `tests/many_agents.rs` | 100 agents with delegated, budgeted tokens: sparse clone, lease, commit, open CR, review by another agent, land (via CR or directly), release; subscribers and a bulk fsck flood alongside. No lost updates, no duplicate lands, every agent action in the audit log and every `ref.updated` event names the human at the root, subscribers saw every version, one winner for a contested exclusive lease, bulk shed while no agent request was refused, changeset and land budgets and the shared delegation-root quota enforced. | 100 agents × 2 rounds | 1,000 × 3 |
| `tests/faults.rs` | Coalescer owner killed mid-batch (every change lands exactly once after client retries, gap-free ref log, a restarted process lands); another server takes a crashed owner's port (it refuses the forwarded lands, the forwarder lands and takes the lease over with a higher fence); metadata store unavailable for a window (ref reads answer the last known version marked `stale`, within `metadata.stale_read_max_ms` and never below `min_version`; every other call a retryable `UNAVAILABLE`; deadlines honoured, lost responses absorbed by idempotent retries, fsck clean); forge database down while the VCS lands (lands, reads and commits work, forge RPCs `UNAVAILABLE`, the outage's lands reach the forge log after recovery); code-search index down (lists and lands keep working). | as listed | same |

A `ServerOptions::faults` injector (wrapping the served metadata and blob stores in
`FaultyMeta` / `FaultyBlob`) is the only server change these tests needed. They run in
parallel within each binary; the `SERIAL` lock they once needed (a crashed owner's port taken
by a parallel test's server) is gone now that forwarded lands verify the owner.

Observed in the gate runs on this machine: linearizability checked 240–280 operations per
variant (up to 22 with unknown outcome under faults) in under 1,000 explored configurations;
100 agents finished 200 lands (100 through change requests) in 11 s while the bulk tier was
shed 448 times and no agent request was refused.

2026-11-18: durable truth capture now records each ref's contiguous ref log, landed-index rows
and successful Land response versions; I1--I4 assert the log chain, one row per ChangeId, each
row's matching log target, and response/version agreement. The opt-in fault hunt previously
stopped at seed 19, preserving `linearizability-truth-19.json`: its `main` row for
`k_...b2c35c...` enters only at version 22, yet that row's source received `ALREADY_LANDED` at
ticks [26, 52], before any call capable of writing the row was invoked. The coalescer now
replaces every planned result with the retryable publication error when an accepted batch fails
to publish; the deterministic regression and a follow-up `BEE_LIN_HUNT=40` run passed all
40 seeds, so unpublished in-batch outcomes are no longer returned.

## The SQLite multi-changeset improvement (done)

`docs/spec/server.md` §12 traced the SQLite hot-ref miss to `F_FULLFSYNC`: every rebased stack
in a coalesced batch committed twice (its trees, its changeset row) before the batch's one ref
CAS, and the next stack read those rows back. Now the batch rebases over
`History::buffered` (a `bee_store::StagedMeta` write buffer that serves its own rows to every
read) and flushes with one `MetaStore::put_batch` transaction (`bee-store-sql` runs the
`put_objects` and `insert_changeset` bodies inside it) before the CAS. A batch costs two
durable commits instead of about two per stack; SQLite went from 73 to 250 lands/s. Write
ordering holds (rows are durable before the ref update that makes them reachable), results
are computed exactly as before, and the linearizability fault test injects `put_batch`
failures and lost responses.

## What G1 (the production gate) still needs

## Event delivery follow-up (2026-11-18)

The 2026-11-15 reference-machine runs (14 cores, 24 GB, under other load) missed the
SPEC §16 event-delivery p99 bound: observed p99 ranged from 2.5 s to 25.5 s, and two
10,000-session runs had 3 and 19 sessions respectively that did not receive every event.
The forge log now has one durable reader and in-process fan-out tail per log, rather than
having every open subscription reread the same page after each append. A consumer that falls
behind the bounded tail receives retryable `UNAVAILABLE` / `subscriber_lagged` with its next
durable sequence in the message, so it can resume instead of silently losing events.

The SPEC §16 event-delivery summary row remains **not re-measured**. The load-harness series
and the p99 ≤ 1 s target must be rerun on the 14-core reference machine before it is marked
as a hit.

G1 (docs/PLAN.md) repeats the metadata benchmark on at least three machines in separate zones
with every write synchronously replicated before it is confirmed. Still to do:

1. **The three-zone setup itself**: Postgres (or the chosen store) primary plus synchronous
   standbys in two other zones (`synchronous_commit = on` / `remote_apply`,
   `synchronous_standby_names` with `ANY 1` or `FIRST 2`), blobs in a replicated bucket, and the
   server in a fourth or co-located zone; record inter-zone RTT.
2. **`bench/db` H1–H3 plus the fault and operations runs** on that setup, and this harness with
   `--pg` against it; the land targets (≥ 100 lands/s with 1,000 landers, p99 time-to-land
   ≤ 10 s, uncontended p99 ≤ 150 ms) must hold with the coalescer.
3. **What this report still finds:**
   - event delivery at 10,000 subscribers missed p99 ≤ 1 s on both stores in every
     2026-11-15 run (3.6–11.7 s), before and after the CreateChangeset fix;
   - every open subscription re-reads the forge event store when the log moves (once per
     bridged batch); at 10,000 sessions delivery holds the 1 s bound with about a third of
     margin and is load-sensitive;
   - stale ref reads come from the serving process's last reads; a cell with a read replica
     could serve them from the replica instead (not needed for the contract).
4. **Availability**: a month of synthetic probes per cell against the 99.99 % / 99.95 % targets;
   a single run can only report its own success ratio.
