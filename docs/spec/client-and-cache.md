# Working copy and ergonomic CLI

Binding for `crates/bee-wc` and the hand-written commands of `crates/bee-cli`
(PRODUCT §4.6, §4.7; SPEC §10.3, §12.4). The node cache (`bee daemon`, SPEC §10.2) is
specified separately when it is built.

## 1. Model

- A working copy is a directory with `.bee/`. There is **no staging area**: every tracked
  file, and every untracked file not ignored by `.gitignore`, belongs to the next commit. Adds,
  deletes and moves are found by comparing the tree with the base manifest (BLAKE3 ids).
- The working copy is based on one changeset, **head**. Unlanded local changes form the
  **stack** on top of **onto**, the target-ref tip they were last synced with. Head is either a
  stack entry or onto.
- All history work is server-side: commit is `CreateChangeset` (delta from the parent, blobs
  uploaded first), sync/restack is `Rebase`, land is `Land` (through the server's coalescer).
  The working copy talks to the server only through the public `bee.v1` API.
- ChangeIds are `BLAKE3("swarm-wc-change\0" ‖ seed ‖ counter)[..16]`; the 32-byte seed is
  drawn once at clone time and stored in the config, so ids are unique across working copies
  and reproducible from the state.

## 2. `.bee/` layout

| File | Content |
|---|---|
| `config.json` | version, endpoint, repo, target ref, seed. Written last at creation: without it the directory is not a working copy. |
| `state.toml` | canonical TOML (keys in declaration order, absent values omitted; a `state.json` left by an older client is read and replaced): head, onto (+ ref version), stack, sparse profile, rebase in progress (done/current/pending changes, conflicts, clean paths, abort state), hidden paths, ChangeId counter, the redaction epoch last applied (SPEC §13: redacted files are the fixed placeholder; mutating commands refresh them) |
| `manifest` | sealed binary (`SWMF` v1, BLAKE3 trailer): every leaf of head inside the sparse profile |
| `statcache` | sealed binary (`SWSC` v1): path → stat tuple + entry, plus the clock it was taken against |
| `clock` | touched at the start of each scan; its mtime is the cache clock |
| `journal/pending.json`, `journal/manifest.next` | write-ahead intent of the operation in progress |
| `oplog.jsonl` | one record per completed operation (state before/after, manifest undo) |
| `objects/<id>` | local blobs the server never saw (rendered conflict files) |
| `conflicts/` | one TOML file per conflicted path of a stopped rebase (`<16 hex of BLAKE3(path)>.toml`: path, kind, base, ours, theirs, rendered, markers, resolved), derived from the state and emptied when the rebase ends |
| `lock` | advisory lock (OS file lock, released if the process dies) |

## 3. Status

Status walks the tree within the sparse profile, honoring nested `.gitignore` files (negation,
anchoring, directory-only patterns, `**`). A cached entry is trusted only when the stat tuple
`(size, mtime_ns, ctime_ns, inode, mode)` matches and the file's mtime is strictly older than
the cache clock; anything else is re-hashed. Status never touches the network.

## 4. Crash safety

Every mutating command does its server work first, then: write `manifest.next`; write
`pending.json` atomically (the commit point); make the working tree match the recorded targets;
install the manifest; write the state; append the oplog; delete the intent. Each step is
idempotent; the next command that finds an intent finishes it and reports
`recovered: <op>` in `bee status`. Fault points for tests: `BEE_WC_FAULT=journal`,
`files:<n>`, `manifest`, `state`.

## 5. Conflicts

Conflicts arrive as data (`MERGE_CONFLICT` with a `ConflictSet`). Content and add/add conflicts
are written with the `diff3-v1` markers (`docs/contracts/diff3-v1.md` §8); in a rebase **ours is
the target side and theirs is the change being moved**. Other kinds write the moved change's
side (or nothing for `file_directory`). `bee resolve` refuses files that still hold a
`<<<<<<< ours` or `>>>>>>> theirs` line unless `--ours/--theirs/--base` picks a side; when all
are resolved the working tree becomes the change (same ChangeId and meta) and the rebase
continues. A resolution that leaves the change empty drops it. `bee sync --abort` restores
the state before the rebase. A conflict on a path outside the sparse profile is refused before
anything is written (`conflict_outside_sparse`).

## 6. Commands

| Command | Behaviour |
|---|---|
| `clone <org/repo> [dir] [--ref] [--sparse P]…` | new working copy of the ref's files |
| `init --repo R [dir]` | adopt an existing directory; existing files are kept as changes |
| `status` | changes, stack, rebase, hidden paths; no network; from the daemon's watcher when one runs (§9) |
| `commit -m M [paths]` | new change after head; changes above head are rebased |
| `amend [-m M] [paths]` | fold changes into head, keeping its ChangeId |
| `describe -m M [--change C]` | new message, same content |
| `diff [paths] [--from R --to R]` | local unified diff, or the server diff |
| `log`, `show`, `blame`, `cat` | history over the API; repo and default revision from the working copy, else `--repo` |
| `sync` (`rebase`) `[--continue \| --abort]` | rebase the stack onto the target tip |
| `resolve [paths] [--ours\|--theirs\|--base]` | mark resolved, continue |
| `land` | land the stack up to head; on conflict, sync and materialize |
| `goto` (`checkout`) `REV` | stack position, ChangeId, `@base` or any revision; keeps unrelated edits |
| `checkout --virtual <org/repo> [dir] [--touch P]…` | sparse-on-demand working copy: nothing on disk until a path is touched (§9) |
| `snapshot REV DIR [--repo R] [--ref] [--sparse P]… [--mode]` | the node daemon writes REV into DIR, then `init` makes it a working copy (§9) |
| `daemon start\|stop\|status\|stats\|prefetch\|pin\|unpin` | run and inspect the per-machine node daemon (§9) |
| `stack list\|new\|edit\|squash\|split` | stack operations |
| `sparse list\|set\|add\|remove` | change the profile and the files on disk |
| `undo`, `oplog` | revert the latest local operation; list operations |
| `cr open\|list\|view` | ChangeService OpenChange / ListChanges / GetChange |
| `auth token` | AuthService MintToken with friendly scope flags |
| `redact (--path P [--at R] [--all-versions] \| --object ID… \| --meta R…) --reason T [--purge-after D \| --now]` | AdminService Redact: every way to name content (SPEC §13), quarantined or purged at once |

Each prints `{"schema": "bee.v1.<words>", "data": …}` with `--json`, described by
`schemas/v1/cli/<words>.schema.json`. `bee auth login` is the generated `AuthService/Login`
call; storing a token is `bee login`.

Endpoint order for working-copy commands: `--endpoint`, `BEE_ENDPOINT`, the endpoint saved in
`.bee/config.json`, `config.toml`, the credentials file.

**Client configuration.** `config.toml` sits in the config directory (`$BEE_CONFIG_DIR`, else
`$XDG_CONFIG_HOME/bee`, else `~/.config/bee`) next to `credentials`, which `bee login`
writes with mode 0600 in a 0700 directory and refuses to read when group or others can.
Keys (all optional; unknown keys are a usage error): `endpoint`, `timeout` (`30s`), `json`,
`daemon` (false = `--no-daemon`), `daemon_socket`, `author`. Each setting resolves as flag,
then environment (`BEE_ENDPOINT`, `BEE_TIMEOUT`, `BEE_JSON`, `BEE_NO_DAEMON`,
`BEE_DAEMON_SOCKET`, `BEE_AUTHOR`), then the file, then the default. Author and time of new changesets: `--author` /
`BEE_AUTHOR` / `$USER`, and `--date` / `BEE_DATE` (Unix seconds) / now.

## 7. Exit codes

0 ok; 1 error; 2 usage; **3** conflicts (markers written, or markers still present at
`resolve`); **4** refused because uncommitted changes would be lost or mixed in (`sync`, `land`,
`undo`, `goto` and `sparse` over dirty paths; `INVALID_STATE` / `dirty_working_copy`, paths in
`payload.paths`); **5** policy denied. Errors raised after the working copy changed (a rebase
stopped on conflicts) carry the operation report in `payload.report`.

## 8. Not covered yet

A file-system driver that materializes a file on first `open()` (cache-tiers.md §7) is not
implemented; the virtual checkout is sparse-on-demand. Windows clients compile but symlinks
are written as files, and the node daemon needs unix sockets.

## 9. The node daemon

The per-machine daemon (`bee-daemon serve`, SPEC §10.2, `docs/spec/cache-tiers.md`) is used
when its socket answers: `$BEE_DAEMON_SOCKET`, else `$XDG_RUNTIME_DIR/bee/daemon.sock`, else
`$HOME/.bee/daemon.sock`. It is switched off by `--no-daemon` or `BEE_NO_DAEMON=1`. A missing
socket, a socket nobody answers on, a daemon that fronts a different server than the endpoint
(`<socket>.upstream`, written by `bee daemon start`) and a daemon that dies mid-fetch
(`UNAVAILABLE`) all fall back to the direct path without an error. `BEE_DAEMON_TRACE=1`
prints the decisions on stderr.

**Objects.** Every `ObjectService/GetObjects` of a working-copy command (`clone`, `init`,
`checkout`, `goto`, `sync`, `sparse`, `cat`, ...) goes to the daemon, which authorizes the
caller upstream, coalesces and caches (the second clone of a repo fetches 0 objects from the
server). Every other call goes to the server.

**Status.** After `clone`, `init`, `checkout --virtual` and `snapshot` the CLI sends `Watch`.
`status` reads `.bee/statcache`, asks `Changes` since its clock and, unless the daemon says
`fresh`, re-examines only the changed paths (cache-tiers.md §8). Otherwise it walks the tree,
and the walk saves the stat cache with its own clock so the next `status` can start from the
watcher. The watcher is not used while the last walk skipped names that are not valid repo
paths (`.bee/daemon-skipped`), nor when a changed path is not a valid repo path. The output
is the same document either way.

**Virtual checkout.** `bee checkout --virtual REPO [DIR] [--touch P]…` creates a working copy
whose sparse profile is `.bee-virtual` (a prefix that names nothing) plus the touched paths.
`bee cat PATH --rev REV` reads any path through the daemon's cache; `bee sparse add PATH`
touches a path (it is written, fetched through the daemon); `status`, `commit`, `sync` and
`land` work on the touched paths and a commit is a delta, so every untouched path keeps its
content on the server. Without a daemon it still works, fetching directly.

**Snapshot.** `bee snapshot REV DIR --repo R` needs the daemon: it materializes REV into DIR
(copy-on-write clones, `--mode hardlink` for read-only files, `--sparse` prefixes), then adopts
DIR as `bee init --repo R --ref REF` does, keeping the files and fetching nothing.
`matchesHead` is false when REV is not the tip of REF, and `status` then shows the difference.

**`bee daemon`.** `start` spawns `bee-daemon serve` detached in front of the endpoint
(`--upstream`, `--cache`, `--max-bytes`, `--no-fsync`; the binary is `$BEE_DAEMON_BIN`, else
next to `bee`, else on PATH), waits until it answers, and records `<socket>.pid`,
`<socket>.upstream` and `<socket>.log`; it does nothing if one runs. `stop` interrupts the
daemon it started (SIGINT, SIGTERM after 5 s). `status` and `stats` read the socket;
`prefetch [--repo] [--rev] PATH…` loads objects, `pin NAME PATH…` / `unpin NAME` keep them
from eviction. Each command has a schema in `schemas/v1/cli/daemon.<verb>.schema.json`.
