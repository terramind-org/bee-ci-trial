# Cache tiers: node daemon, regional cache, snapshots, watcher, virtual checkout

Binding for `crates/bee-daemon` and for `ObjectService/GetDaemonSnapshot` in `bee-server`
(`crates/bee-server/src/services/daemon.rs`). Implements PRODUCT §4.5 and the cache parts
of §4.6, SPEC §10.2 (node cache), §6.3 (`min_version`), §6.5 (tiers in front of cold storage)
and §10.5 (signed URLs). Working-copy behaviour stays in `docs/spec/client-and-cache.md`.

## 1. Tiers

```
working copies, agents, CI jobs ──unix socket──▶ node daemon ──▶ regional cache ──▶ server
                                                (disk cache)      (memory cache)
```

- One binary, `bee-daemon`. `serve` runs the **node daemon**: a disk cache on a unix socket
  (default `$BEE_DAEMON_SOCKET`, else `$XDG_RUNTIME_DIR/bee/daemon.sock`, else
  `~/.bee/daemon.sock`), one per machine. `regional` runs the **regional cache**: the same
  tier on TCP with an in-memory store, for a region.
- A tier talks only to the tier directly above it (`--upstream`), over the Connect protocol
  with binary protobuf. It is configured with the URL of the server or of a regional cache;
  nothing else changes.
- Every tier answers `bee.v1.ObjectService` with the server's request and response
  messages (§3). Every other `bee.v1` RPC is forwarded byte for byte, with the caller's
  `Authorization` header, so a client can point any `bee.v1` client at a tier.
- `ServerInfo.tier` is `daemon` or `cache`; `ServerInfo.features` gains `tier_cache`.
  `TierStats.tier` is `daemon` or `regional`. Responses carry `bee-tier` metadata and the
  last `bee-policy-epoch` / `bee-redaction-epoch` seen for the repo; the server's own
  answers carry `bee-tier: server` (a trailer). `GetServerInfo` naming a repo is always
  asked of the tier above with the caller's credential, so its epochs are current.

## 2. Object store

Objects are keyed by id only, so identical content from any repo or org is stored once.
Bytes are verified against the id (BLAKE3 over the bytes, SPEC §3.1) when written **and**
when read; a store never returns bytes that do not match. Eviction is LRU by a logical clock
between watermarks: above `max_bytes` the oldest unpinned objects go until the store is at
the low watermark (90% by default); the most recently used object is never chosen.

**Pins.** In-memory pins (held while a snapshot links files, or for the length of an
operation) and named, persistent pin sets (`Pin {name, repo, revision, sparse}`) both keep
objects from eviction.

**Disk layout** (node daemon):

| Path | Content |
|---|---|
| `objects/<tag>/<hh>/<64 hex>` | one object, mode `0444`; `<tag>` is the id's text tag (`b1`, `t1`, …) |
| `tmp/` | writes in progress; emptied at startup |
| `index` | recency index, `SWDI` v1: magic, version u32, count u64, then `(id 34, size u64, tick u64)` per object, BLAKE3 trailer; replaced atomically every 4,096 mutations, every 30 s and at shutdown |
| `pins.json` | named pin sets, replaced atomically |

Writes follow SPEC §10.2: temp file → verify → fsync (on by default, `--no-fsync` to skip)
→ rename. **Crash safety:** the object files are the truth and the index only remembers
recency. At startup `tmp/` is emptied, the index is loaded if its trailer verifies, and it is
reconciled with the files: unknown files are adopted as least recently used, entries without
a file are dropped. A torn or corrupt file is found on its first read, deleted, and fetched
again. Fault points (`put.write`, `put.rename`, `put.index`, `index.write`) let tests crash at
each step.

**Regional store:** the same LRU in memory, bounded by `--memory-bytes`, with no long-lived
state (lost on restart).

## 3. Read path and authorization

A tier never decides on its own that a caller may see an object. Bytes are shared; decisions
are per `(credential, repo, object)` and come only from the tier above:

1. An object the tier above served to this credential in this repo is allowed.
2. A cached object without a decision is checked with one `HasObjects` per 256 ids; `true`
   is a decision, `false` sends the ids through `GetObjects` so the caller gets the tier
   above's exact per-item answer (`NOT_FOUND`, `REDACTED`, …) — never the cached bytes.
3. Decisions are trusted for `confirm_ms` (default 5,000) after the last successful upstream
   call for that credential and repo; after that they are re-made. Every decision, access
   class, cached ref and listing of a repo is dropped when a response shows a new policy or
   redaction epoch for it.
4. If the tier above cannot be reached when a decision is needed, the read fails with
   `UNAVAILABLE` (**fail closed**, counted in `fail_closed`). Within `confirm_ms` of the last
   confirmation the cache keeps serving; so token revocation, policy changes and redactions
   reach cached reads within `confirm_ms`, which the redaction propagation bound (SPEC §13,
   ≥ `signed_url_ttl`) covers at the default. Repo-access changes (SPEC §9.6: visibility,
   archive, delete, transfer, membership) reach cached reads within `confirm_ms` plus the
   server's `forge.access_cache_ms`, since every re-made decision asks the server, which
   decides them afresh.
5. An upstream `REDACTED` answer removes the bytes from the cache at once.

The daemon keeps only a BLAKE3 digest of each credential, never the credential. It does not
verify tokens locally and does not use SPEC §10.2's optional "root access class after one
repo-read decision" shortcut: tokens can be attenuated to paths, and the tier cannot see that
without verifying the token. Both are stricter than SPEC §10.2's description and need no
amendment (the shortcut is a MAY).

**Coalescing and batching.** Concurrent misses for one object share one upstream fetch, and
concurrent checks for one `(credential, repo, object)` share one `HasObjects`. A request's
misses go up in one `GetObjects` per 256 ids; at most `upstream_concurrency` (32) requests
are in flight; retryable failures (`retryable` in the error detail, SPEC §15.5) are retried at
most 3 times with backoff from 20 ms (honouring `retry_after_ms`). Callers whose leader was
another credential reuse the bytes and only add a decision.

**Signed URLs.** The tier asks the tier above for signed URLs (`allow_signed_urls`, SPEC
§10.5), downloads them, and verifies the bytes; a URL that fails or bytes that do not verify
fall back to a plain `GetObjects` for those ids. Bytes that fail verification twice are an
`INTEGRITY` item. A tier always answers its own callers with bytes, never with URLs (allowed:
the server *may* answer with a URL).

**Responses** are built as the server builds them: 1 MiB chunks, `resume_offset` / `length`
applied the same way, `total_size`, `encoding = RAW`, the access class the tier above last
reported for that object in that repo. When the class is unknown (bytes that arrived through
another repo, or before a restart) the tier reports `ACCESS_CLASS_RESTRICTED`, the
conservative class. Malformed or oversized (>256 ids) requests are forwarded unchanged so the
error is the server's.

**Refs.** `ResolveRef` answers are cached per credential for `ref_ttl_ms` (default 1,000).
A request whose `min_version` is above the cached version is read from above (read your
writes, SPEC §6.3); concurrent refreshes coalesce; a cached ref never moves backwards (a
lagging answer loses to a newer one already served). `at_version` answers are immutable and
cached while decisions are confirmed. `HasObjects` is always forwarded.

## 4. Daemon-local API

`POST /daemon/v1/<op>` with a JSON body, on the same socket; errors are
`{"error": <ErrorDetail>}` plus the Connect error fields, with the reason's HTTP status.

| Op | Body | Effect |
|---|---|---|
| `Stats` | `{}` | counters: hits, misses, hit rate, fail-closed, upstream requests, signed-URL fetches and fallbacks, ref hits, store size, evictions |
| `Prefetch` | `{repo, revision, sparse[]}` | resolve, list and cache every blob of the scope |
| `Snapshot` | `{repo, revision, dest, sparse[], paths[], mode, verify, intoExisting}` | §5 |
| `Tree` | `{repo, revision, prefix}` | the full-tree index of a revision for this caller (§7) |
| `Pin` / `Unpin` / `Pins` | `{name, repo, revision, sparse[]}` / `{name}` / `{}` | persistent pin sets |
| `Bootstrap` | `{repo, sparse[]}` | §6 |
| `Watch` / `Unwatch` / `Changes` | `{root}` / `{root}` / `{root, sinceNs}` | §8 |
| `Flush` | `{}` | write the recency index |

The socket is mode `0666`: any local user may read objects with their own token.
`Snapshot`, `Watch`, `Unwatch`, `Changes`, `Pin`, `Unpin` and `Flush` touch the local file
system or the shared cache's policy and are accepted only from the daemon's own user (peer
credentials of the unix socket); over TCP (regional) they are refused (`PERMISSION_DENIED`).
`GET /healthz` and `GET /metrics` (Prometheus text, `bee_cache_*{tier=…}`) are served too.

## 5. Snapshots

`Snapshot` materializes a revision into a directory from the cache: resolve, list (listings
of a changeset are cached per credential), make every object cached and authorized (§3),
pin them, then create the files with one thread per 512 files (at most 8):

- `clone` (default): `std::fs::copy`, which is `fclonefileat` (copy-on-write) on APFS and
  `copy_file_range` (reflink on Btrfs/XFS) on Linux; ordinary writable files.
- `hardlink`: a hard link to the cache file, for read-only consumers. Files are `0444` and
  share the cache inode: in-place edits are refused by the mode, and write-then-rename editors
  replace the link without touching the cache (whose reads verify anyway). Executables are
  cloned, because a link would share the cache file's mode.
- `copy`: read and write.

The destination must be empty (or `intoExisting`, which only adds missing files and refuses
to write through anything that is not a directory, SPEC §10.3). Paths colliding under ASCII
case folding on a case-insensitive destination are skipped and reported, as are restricted
and unreadable paths. To turn a snapshot into a working copy, run `bee init` on it
(`bee_wc::create` with `adopt: true`): files already in place are kept and nothing is
fetched.

## 6. Bootstrap from server snapshots

`ObjectService/GetDaemonSnapshot {repo, sparse_prefixes}` (server): the readable blobs of
`main`'s tip under the prefixes, as **the caller** may read them (restricted paths left out,
redacted objects skipped), encoded as `SWSN` v1 — `"SWSN"`, version u32 LE, count u64 LE,
then `(id 34, length u64 LE, bytes)` per object in id order, then BLAKE3 of all of that. The
file is stored content-addressed in the blob store and returned by signed URL with its
BLAKE3 `digest`, `size_bytes`, `root_changeset`, `ref_version` and `redaction_epoch`. Without
signed URLs the RPC answers `UNIMPLEMENTED`; a scope over 1 GiB answers `TOO_LARGE`.
Snapshot files have no object row, so GC removes them once they are older than its upload
window.

`Bootstrap` (daemon) downloads the URL, checks size and digest, the trailer and every object
against its id, and only then loads the objects. It moves bytes only: callers still need
decisions (§3), so an object redacted after the snapshot was built is refused upstream and
evicted on first use. `serve --bootstrap ORG/REPO [--bootstrap-sparse P]…` bootstraps at
start and every `--bootstrap-every-secs` (default 3,600) as the principal of `BEE_TOKEN`.

## 7. Virtual checkout: sparse-on-demand

macOS offers no way to materialize a file on first `open()` without a file-system driver
(a kernel extension, FSKit, or an NFS/FUSE server), and none of those is available as a
pinned, pure-Rust, `unsafe`-free dependency today. So the virtual checkout is
**sparse-on-demand**, built from parts that exist:

- The working copy is a normal bee working copy whose sparse profile holds only touched
  paths. A new one starts with the profile `.bee-virtual` (a prefix that names nothing), so
  nothing is on disk.
- The full tree is known through the daemon's index (`Tree`), and any path's content is read
  from the daemon cache by id (`GetObjects`) without materializing it.
- Touching a path materializes it: add it to the profile (`bee sparse add`,
  `WorkingCopy::set_sparse`), which fetches through the daemon.
- `status`, `commit`, `land`, `sync` operate on the materialized paths; because commits are
  deltas from the parent, every untouched path keeps its content on the server.

What is **not** virtual: tools that open files directly see only materialized paths. A file
created at an unmaterialized path is outside the profile and ignored by `status` until its
path is added. Opening on access needs a file-system driver; when a pure-Rust FUSE or NFSv3
server is pinned and audited it can be added behind a cargo feature serving the same index
and cache. `bee checkout --virtual` and `bee cat` through the daemon are this composition
in `bee-cli` (`docs/spec/client-and-cache.md` §9; `crates/bee-cli/tests/daemon.rs`);
`crates/bee-daemon/tests/e2e.rs` holds the reference version.

## 8. Watcher status

`Watch {root}` starts a `notify` watcher (FSEvents on macOS, inotify on Linux) on a working
copy. The daemon journals each changed path with the wall time its event arrived.
`Changes {root, sinceNs}` returns the paths whose events arrived at or after `sinceNs` minus
2 s of slack (a superset), or `fresh: true` with a reason when the journal cannot vouch for
the whole interval: the watcher started after `sinceNs − 2 s`, the OS dropped events (rescan
flag), the watcher failed, or the sync cookie did not return within 3 s. Before answering,
the daemon writes `.bee/daemon-cookie-<n>` and waits for its event, so every change made
before the call is journaled. Events under `.bee/` are not reported; non-UTF-8 paths force
a fresh answer.

**Status from the journal** (what `bee status` does when a daemon watches the root; the
reference implementation is `watched_status` in `crates/bee-daemon/tests/common/mod.rs`):
read `.bee/statcache`; ask `Changes` with `sinceNs` = its clock; on `fresh`, run the normal
walk. Otherwise touch the clock, map a changed `.gitignore` to its directory, drop the stat
entries of changed paths (forcing a rehash), run `bee_wc::scan::scan` with a sparse profile
of just the changed paths, merge with the remaining stat entries (filtered by the working
copy's profile, plus hidden and gitlink entries), save the stat cache with the new clock, and
diff against the base manifest. This is sound because any change to a file after the old
clock produces an event at or after it, so every entry not rescanned is unchanged since it
was hashed.

## 9. Measurements

Debug builds (the repository forbids release builds in agent runs) on a shared Apple-silicon
laptop with a load average of 30–40 from other jobs; APFS. Absolute times are pessimistic;
the counts are exact.

| Test | Result |
|---|---|
| `tier.rs::fetch_coalescing_1000_concurrent_requests_one_upstream_fetch` | 1,000 concurrent requests × 10 objects → **1** upstream `GetObjects`, 212 ms (200 ms of it the simulated upstream latency) |
| `tier.rs::snapshot_of_a_10k_file_tree` (10,001 entries, 2.1 MB) | cold (fetch into the cache + clone): 4.9–9.6 s over three runs (list 20–30 ms, fetch 3.2–5.4 s, materialize 1.3–4.2 s); warm: clone 1.5–3.2 s, hardlink 2.0–2.3 s, copy 1.1–1.3 s |
| `tier.rs::hundred_agents_snapshot_the_same_revision_with_one_fetch_per_object` | 100 concurrent agents × 300-file hardlink snapshots; each object fetched upstream **once** (301 fetches); 8.0 s for all 30,000 files (all 100 run concurrently; per agent: fetch 105 ms, materialize 7.7 s) |
| `e2e.rs::as_cache_01_…` (AS-CACHE-01) | 50 jobs × 1,000-file scope: **1** fetch per object (1,000 ids in 4 `GetObjects`, 4 `HasObjects` for the second principal), hit rate 0.98, 40 s wall for all 50 jobs; per job: bee-wc clone through the daemon 39.9 s, daemon snapshot + `init` 24.2 s (all 50 in parallel) |
| single 1,000-file clone (no contention) | direct 5.9 s, through the daemon 5.4 s cold / 4.7 s warm — dominated by bee-wc's per-file `F_FULLFSYNC`; daemon snapshots do not fsync each file |
| `watch_status.rs::watcher_status_equals_a_full_walk_under_randomized_edits` | 40/40 rounds answered from the watcher journal, all equal to a full rehash (13 edit kinds incl. same-size same-instant rewrites, directory renames, `.gitignore` toggles, mode flips, symlinks) |
| `watch_status.rs::watcher_status_rescans_only_changed_paths_on_a_large_tree` | 3,003 files, one edit: watcher status rescans 1 path in 53 ms (mostly the FSEvents cookie round trip), the stat-cache walk takes 16 ms. At this size the walk is cheaper; the walk grows with the tree, the watcher with the change |
| `bootstrap.rs::…` | 300-object scope bootstrapped in 98 ms (17 KB bundle); the following clone fetched **0** objects |

## 10. Not covered yet

- The CLI routes through the daemon for object reads, watcher status, `checkout --virtual`,
  `snapshot` and `bee daemon ...` (`client-and-cache.md` §9). It does not route `HasObjects`
  or ref reads through it, and it trusts the daemon's upstream only when `bee daemon start`
  recorded it (`<socket>.upstream`).
- The 99% hit-rate target per tier on the reference CI workload needs the load harness.
- A tier serves only `ObjectService` from cache; listings (`ListTree`) are forwarded except
  the daemon's own `Tree`/`Snapshot` listings.

## 11. Configuration

`bee-daemon serve` and `bee-daemon regional` read one TOML file, `--config FILE` or
`$BEE_DAEMON_CONFIG`, with a single `[daemon]` table. Every key is optional; unknown keys
and bad values stop the daemon before it starts. A string that is exactly `$NAME` or `${NAME}`
is the environment variable `NAME` (unset is an error), `$$` escapes a literal `$`.
Precedence: defaults, the file, the environment (`BEE_ENDPOINT` for the upstream), flags.
`--print-config` prints the effective settings as JSON and exits.

| Key | Default | Flag |
|---|---|---|
| `upstream` | — | `--upstream` |
| `socket` | §4 socket path | `--socket` |
| `listen` | — (regional) | `--listen` |
| `cache_dir` | `$XDG_CACHE_HOME/bee/daemon` | `--cache` |
| `max_bytes` | 10 GiB | `--max-bytes` |
| `low_watermark_pct` | 90 | |
| `fsync` | true | `--no-fsync` |
| `memory_bytes` | 4 GiB (regional) | `--memory-bytes` |
| `signed_urls` | true | `--no-signed-urls` |
| `confirm_ms` | 5000 | `--confirm-ms` |
| `ref_ttl_ms` | 1000 | `--ref-ttl-ms` |
| `upstream_concurrency` | 32 | `--upstream-concurrency` |
| `max_retries` | 3 | |
| `bootstrap` | `[]` | `--bootstrap` (repeatable; replaces the list) |
| `bootstrap_sparse` | `[]` | `--bootstrap-sparse` |
| `bootstrap_every_secs` | 3600 (at least 60) | `--bootstrap-every-secs` |
