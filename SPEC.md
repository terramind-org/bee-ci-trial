# bee / Beehive Specification

Status: **draft v0.2 (phase 0); scope per docs/PRODUCT.md**. This document is the contract every crate builds against.
When code and spec disagree, the code is wrong or the spec gets amended in the same change
(see `docs/contracts/amendments.md`).

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in RFC 2119.

---

## 1. What this is

**bee** is a version control system for always-online clients: coding agents, CI jobs and
the people who direct them. **Beehive** is the forge built on it: orgs, repos, change
requests, review, checks, leases and events. Both ship as one server binary, `bee-server`,
and one client binary, `bee`.

The design starts from five assumptions that differ from git:

1. **Clients are online.** History, merges, rebases and landing happen on the server. Clients
   hold a bounded cache and a working copy of only the paths they need, or no working copy at all.
2. **Every question has an API.** Log, blame, diff, ancestry, merge base, conflicts, review
   state: all are RPCs with typed, versioned responses. Nothing requires a local clone.
3. **Cost scales with the change, not the repo.** Committing one file touches O(depth) objects.
   Landing never takes a global lock. Checkout cost scales with the sparse scope.
4. **Agents are first-class principals.** Every write records who did it, on whose behalf,
   with which model and session, and under which narrowed permissions.
5. **Collisions are prevented, not just resolved.** Agents announce the paths they intend to
   touch (leases), conflicts are structured data an agent can resolve, and review effort goes
   to the risky part of a change.

### 1.1 Scope

Scope and the definition of done live in [`docs/PRODUCT.md`](docs/PRODUCT.md): everything in
Anthropic's post, everything in the Scion reference, and all of GitHub, with nothing deferred.
This document specifies the technical contract. v0.2 covers the core (objects, storage, write
path, merge, derived data, fetch, identity, forge basics, reliability, performance, migration);
every area of PRODUCT.md §4 that is not yet specified here is tracked row by row in
`docs/coverage/features.csv` and gets its own section or companion document before it is built.

### 1.2 Design boundaries

- Native clients assume the server is reachable. Offline work goes through the git gateway (§17).
- No IDE or code completion; Beehive provides APIs, the agent runner and a model gateway.

---

## 2. System overview

```text
            agents / CI / humans
   ┌──────────────┬──────────────┬───────────────┐
   │ bee CLI    │ MCP clients  │ Beehive web  │
   │ (--json)     │ (bee mcp)  │ (React)       │
   └──────┬───────┴──────┬───────┴───────┬───────┘
          │ unix socket  │               │  HTTPS (Connect: gRPC / JSON)
          ▼              │               │
   ┌──────────────┐      │               │
   │ node cache   │──────┴───────────────┤
   │ (bee daemon)                      ▼
   └──────────────┘             ┌──────────────────────────────┐
                                │ bee-server                 │
                                │  VCS core · Beehive forge   │
                                │  policy · auth · events      │
                                └──────┬───────────────┬───────┘
                                       │               │
                              metadata store      blob store
                         (Postgres | SQLite)   (S3-compatible | disk)
```

### 2.1 Components

| Component | Crate(s) | Responsibility |
|---|---|---|
| Domain | `bee-core`, `bee-object` | Ids, errors, principals, paths, canonical encoding, objects |
| Storage | `bee-store`, `bee-store-sql`, `bee-store-blob` | Traits, Postgres/SQLite metadata, S3/disk blobs, write ordering |
| Algorithms | `bee-merge`, `bee-history`, `bee-derived` | Diff3, graph queries, derived indexes |
| Security | `bee-policy`, `bee-auth` | Policy-as-code, principals, delegated tokens |
| Forge | `bee-forge` | Orgs, repos, change requests, reviews, checks, leases, events, provenance |
| Protocol | `bee-proto` | `bee.v1` protobuf types and domain conversions |
| Server | `bee-server` | The single server binary |
| Client | `bee-cli`, `bee-wc`, `bee-daemon`, `bee-mcp` | CLI, working copy, node cache, MCP server |
| Git and GitHub | `bee-git`, `bee-import`, `bee-github` | Git object format and smart HTTP, git history import, GitHub importer and dual-run bridge |
| Verification | `bee-testkit`, `bee-conformance` | Fixtures, fault injection, reference model, differential tests |

Crate layering is enforced (`scripts/layers.toml`): a crate may depend only on crates in a
lower layer. The layers are listed in `docs/PLAN.md`.

---

## 3. Identifiers

### 3.1 Object ids

An object id is 34 bytes:

| Byte(s) | Meaning |
|---|---|
| 0 | Hash algorithm. `0x01` = BLAKE3-256. `0x02` = foreign git object id (gitlinks only, §4.2). Other values reserved. |
| 1 | Object kind: `0x01` blob, `0x02` tree, `0x03` changeset, `0x04` meta. `0x10`–`0x7F` reserved for derived objects. `0x00` = foreign, valid only with algorithm `0x02`. |
| 2–33 | 32-byte digest |

- A **blob** digest is BLAKE3 over its raw bytes. Every other kind's digest is BLAKE3 over its
  canonical encoding (§4).
- The kind is part of the id, so ids of different kinds never collide, and a reader always
  knows what it is fetching.
- Algorithm and kind must agree: algorithm `0x01` with kinds `0x01`–`0x04` or `0x10`–`0x7F`;
  algorithm `0x02` with kind `0x00`. Any other pair is rejected (`INVALID_ARGUMENT`/`INVALID_ID`).
- **Text form:** `<kind-letter><algorithm-digit>_<64 lowercase hex>`, e.g. `c1_9f3a…`. Kind
  letters: `b` blob, `t` tree, `c` changeset, `m` meta. Clients MAY abbreviate changeset ids to
  a unique prefix of at least 10 hex digits; the server resolves prefixes per repo.
- **Derived kinds** are written `d1<kk>_<64 hex>`, where `<kk>` is the kind byte as two
  lowercase hex digits (`d110_…` to `d17f_…`). A **foreign** id is written `g2_<64 hex>`.
- Hex is always lowercase; uppercase digits are rejected. A prefix has 10 to 64 digits and may
  have an odd length (it matches by nibble). Resolving a prefix that matches no visible object
  gives `NOT_FOUND`; one that matches several gives `AMBIGUOUS_PREFIX` with the matching ids,
  sorted, in the payload under `candidates`.

### 3.2 Surrogate ids

Orgs, repos, principals, change requests, reviews, checks, leases and events use **ULIDs**
(128-bit, time-ordered, Crockford base32 in text). Names are mutable labels that point at
surrogate ids, so renaming an org or repo never rewrites a key.

- Text is 26 characters, written in uppercase; parsers accept either case. The letters `I`,
  `L`, `O` and `U` are rejected (no aliasing), and the first character must be `0`–`7`.
- The top 48 bits are milliseconds since the Unix epoch, the low 80 bits random. Domain code
  receives both from an injected clock and random source, never reading them itself.

### 3.3 ChangeId

A ChangeId is 16 random bytes chosen by the client when a change is first created, and kept
across amend and rebase. It is the stable identity of "this piece of work" for review,
landing idempotency and leases. Text form: `k_<32 lowercase hex>`.

- ChangeIds are scoped to a repo: the server indexes `(repo_id, change_id)`.
- Among changesets landed on one ref, a ChangeId MUST be unique (§7.4).
- The importer derives ChangeIds as the first 16 bytes of BLAKE3(`"swarm-import\0" || git_sha`),
  where `git_sha` is the commit's raw object id (20 bytes for SHA-1, 32 for SHA-256), not hex.
- The all-zero ChangeId is reserved for the empty root changeset (§4.4).

---

## 4. Canonical encoding

Structured objects are encoded as **deterministic CBOR** using a strict profile ("sCBOR v1"),
so third-party implementations can rely on standard CBOR libraries.

- Every structured object is a CBOR map with **unsigned-integer keys**, encoded in
  core deterministic form (RFC 8949 §4.2.1): shortest-form integers and lengths, definite
  lengths only, map keys sorted by their encoded bytes, no floats, no tags, no indefinite items.
- Key `0` is always `format_version` (unsigned). v1 objects use `1`.
- Keys `1`–`63` are defined by this spec per object kind.
- Keys `≥ 64` are **extensions**. An **odd** extension key is *critical*: a decoder that does
  not know it MUST reject the object. An **even** extension key is *ignorable*: decoders keep
  it byte-for-byte and it still counts toward the id.
- Byte strings are CBOR byte strings (major type 2); text that is not guaranteed to be
  UTF-8 (paths, names) is a byte string. Human messages are text strings (major type 3) and
  MUST be valid UTF-8.
- Decoders MUST reject: non-shortest forms, unsorted or duplicate keys, unknown
  `format_version`, unknown odd extensions, trailing bytes, and any value exceeding the limits
  in §4.6. The result: one logical value has exactly one byte encoding, so one id.

Profile details (normative):

- Only major types 0–5 appear: unsigned and negative integers, byte strings, text strings,
  arrays and maps. Major type 6 (tags) and 7 (floats, `true`/`false`/`null`, other simple
  values) are rejected, as are additional-information values 28–31.
- Every map, at every nesting level, has unsigned-integer keys. For such keys, numeric order is
  the same as encoded-byte order.
- Unknown keys `1`–`63` in a top-level object are rejected (`INVALID_FIELD`); new fields in
  that range come with a new `format_version`.
- Nested maps (tree entries, delta entries and put ops, provenance, checks, cost) have no
  `format_version` and take no extensions: any key they do not define is rejected. Only
  top-level objects carry extensions.
- Required fields are always present, even when they are empty arrays (`entries`, `parents`,
  `delta`). Optional fields are omitted when absent, never encoded as `null`. Optional arrays
  are omitted when empty, and a present but empty optional array is rejected.
- Arrays and maps nest at most 32 deep.
- Error subcodes: malformed CBOR or a profile violation gives `NON_CANONICAL`; an unknown
  `format_version` gives `UNKNOWN_FORMAT_VERSION`; an unknown odd extension gives
  `UNKNOWN_CRITICAL_EXTENSION`; a missing, unknown or mistyped field gives `INVALID_FIELD`;
  breaking a limit in §4.6 gives `TOO_LARGE` (all others are `INVALID_ARGUMENT`, §12.2).

Golden vectors for every object kind live in `testdata/vectors/` and are frozen once published.
A file is never edited; new vectors go in a new file.

### 4.1 Blob

Raw file bytes: regular file contents, or a symlink's target. Blobs are not CBOR. Stored
compressed (zstd); the compression level is not part of identity. Default max size 100 MiB
(configurable per repo up to 4 GiB). Chunked large blobs are reserved for a later format version.

### 4.2 Tree

| Key | Field | Type |
|---|---|---|
| 1 | `entries` | array of entry maps, sorted bytewise by `name`, names unique |
| 64 | `git` | ignorable extension: the git tree note (§17.4), import only |

Entry: `{1: name (bytes), 2: mode (uint), 3: id (34 bytes), 4: size (uint)}`.

- `mode`: `1` file, `2` executable, `3` symlink, `4` tree, `5` gitlink (a submodule pointer;
  `id` is then a foreign id `0x02 0x00 <20 or 32 byte git object id, zero-padded>` and `size` is 0).
- `size`: blob length for files and symlinks; `0` for trees and gitlinks. Lets a virtual
  filesystem `stat` without fetching.
- `id` kind must match `mode`: a blob id for modes 1–3, a tree id for mode 4, a foreign id for
  mode 5 (`WRONG_KIND` otherwise).
- `name` MUST NOT be empty, longer than 255 bytes, `.` or `..`, contain `/` or NUL, or equal
  `.bee` or `.swarm` under ASCII case-insensitive comparison.
- Non-root trees MUST NOT be empty. The root tree MAY be empty. A directory whose last leaf is
  deleted disappears from its parent.

### 4.3 Meta

Kept apart from the changeset so it can be redacted without changing any changeset id.

| Key | Field | Type |
|---|---|---|
| 1 | `author` | text |
| 2 | `author_time` | `[seconds (int), tz_offset_minutes (int)]` |
| 3 | `committer` | text, optional (import only) |
| 4 | `committer_time` | optional (import only) |
| 5 | `creator` | principal id (text), set by the server, never by the client |
| 6 | `message` | text |
| 7 | `provenance` | provenance map (§10.4), optional |
| 64 | `git` | ignorable extension: the original git commit's header bytes (§17.4), import only |

- Times are an array of exactly two integers; `seconds` fits a signed 64-bit integer and
  `tz_offset_minutes` a signed 32-bit integer. `committer_time` has the same shape.
- `author`, `creator` and `message` are required (the empty root's are empty strings).
- Provenance (§10.4) is a map with keys `1`–`7`. `principal_chain` is an array of text;
  `runtime`, `model` and `session` are text; `prompt_digest` is exactly 32 bytes;
  `checks_run` is an array of `{1: name (text), 2: result (text), 3: duration_ms (uint)}`, all
  three required; `cost` is `{1: input_tokens, 2: output_tokens, 3: usd_micros}`, each an
  optional uint. `principal_chain` and `checks_run` are omitted when empty. An empty provenance
  map is valid and distinct from no provenance.

### 4.4 Changeset

| Key | Field | Type |
|---|---|---|
| 1 | `parents` | array of changeset ids, ordered, unique |
| 2 | `root` | tree id |
| 3 | `meta` | meta id |
| 4 | `change_id` | 16 bytes |
| 5 | `delta` | array of delta entries, sorted bytewise by path, paths unique |

Delta entry: `{1: path (bytes), 2: op}` where op is `{1: mode, 2: blob id, 3: size}` (put) or
the unsigned integer `0` (delete). Paths are `/`-separated, relative, normalized (§4.5).

- A put's mode is any mode but `4` (tree); its id and size follow the tree-entry rules (§4.2),
  so a gitlink put carries a foreign id and size 0.
- Checkable without trees (`INVALID_DELTA`): no two puts where one path lies inside the other
  (it cannot be a file and a directory afterwards); no two deletes where one path lies inside
  the other; a parentless changeset has only puts.
- "File-level" means leaves (modes 1, 2, 3, 5). Replacing a directory with a file deletes every
  leaf under it and puts the file; replacing a file with a directory deletes the file and puts
  every new leaf. A put that does not change its path, or a delete of a path that is absent or
  a directory, is not part of any difference and is rejected.

**Invariant D:** `delta` MUST equal the exact file-level difference from the root tree of
`parents[0]` to `root` (or from the empty tree if there are no parents). The delta is redundant
with the trees, so it can be checked; it is also what makes ingest and rebase O(change). A
violation found by comparing against the trees is `INVALID_ARGUMENT`/`DELTA_MISMATCH`, with the
first differing path in the payload under `path`.

Each repo has exactly one parentless changeset created by the server, the **empty root**
(empty tree, fixed meta). The importer MAY create additional parentless changesets. The empty
root is fully fixed, so its id is the same in every repo (golden vector
`changeset-empty-root`): no parents, the empty tree, the meta
`{0: 1, 1: "", 2: [0, 0], 5: "", 6: ""}`, the all-zero ChangeId and an empty delta.

### 4.5 Path rules

- A path is a non-empty byte string of `/`-separated components, each satisfying the tree-name
  rules in §4.2. No leading or trailing `/`, no empty components.
- No Unicode normalization; bytes are compared bytewise.
- Max path length 4096 bytes; max depth 256 components; max component length 255 bytes.
- Any violation, length and depth included, is `INVALID_ARGUMENT`/`INVALID_PATH`.

### 4.6 Limits

| Limit | Default |
|---|---|
| Blob size | 100 MiB (repo-configurable, hard cap 4 GiB) |
| Tree entries | 1,000,000 |
| Delta entries per changeset | 1,000,000 |
| Parents per changeset | 16 |
| Message | 1 MiB |
| Encoded tree / meta / changeset | 256 MiB |

### 4.7 Global invariants

Four invariants hold at every instant, for every reader, through crashes, retries, redaction and
garbage collection. fsck (§14.5) checks each one; a violation is a bug, never a state to work
around.

1. **Bytes match ids.** Stored bytes of every live object and changeset hash to their id and are
   the canonical encoding (fsck: `digest_mismatch`, `changeset_id_mismatch`, `non_canonical`).
2. **References resolve.** A changeset row exists only when every object it references (root
   tree, meta, delta puts, subtrees) and every parent already has a row in the repo, and a ref
   or ref-log entry targets only changesets that have rows (§6.2; fsck: `missing_parent`,
   `dangling_reference`, `dangling_ref`, `object_path_missing`).
3. **Nothing is mutated except the listed mutable state:** ref tips and versions (by CAS, with
   one appended ref-log entry per step), object state (live, quarantined, purged) and the
   redaction records and epochs that go with it, a changeset's `last_used_ms`, upload records,
   and the deletions garbage collection and purge make (§6.4, §13). Changeset rows, object
   rows' sizes and inline bytes, `object_paths` rows, eager derived rows, ref-log entries and
   landed rows are written once and never rewritten.
4. **Ids never change.** No operation (amend, rebase, land, redaction, purge, GC, import or
   export) changes the id of an existing object or changeset; redaction removes bytes, not
   ids, and a changeset that carries a redacted entry keeps its id (§13).

---

## 5. Refs

A ref is `{repo_id, name, target: changeset id, version: u64}`.

- `version` starts at 1 and increases by exactly 1 per update. `name@version` addresses ref
  history. Deleting and recreating a ref continues its numbering.
- Every update appends a **ref-log** entry: `{old, new, version, principal, time, landed: [(change_id, source, result)]}`.
- **Ref namespaces:**
  - `main`, `release/*`: protected; written only by Land (§7.4) under policy.
  - `ws/<principal>/<name>`: a principal's own workspaces. Writable by that principal (and
    anyone it has delegated to) with plain CAS. Agents use these to park work, share
    partial results and hand off without opening a change request.
  - `green/*`: written by CI to mark tested tips (`green/<ref>`, §11.6), with plain CAS. CI
    starts no workflows for these refs.
- Work in progress needs no ref at all: an unreferenced changeset is addressed by id and kept
  alive by an open change request, a workspace ref, or the draft retention window (§6.4).

**Ref name grammar.** A ref name is 1 to 255 bytes of UTF-8, split on `/` into components.
Violations are `INVALID_ARGUMENT`/`INVALID_REF_NAME`.

- No component is empty (so no leading, trailing or doubled `/`), starts with `.`, or ends with
  `.` or `.lock`.
- The name contains no `..`, no ASCII control character, no space, and none of
  `~ ^ : ? * [ \ @`. These characters are revision syntax or glob characters.
- The whole name is not id-shaped: an id tag (`b1`, `t1`, `c1`, `m1`, `g2`, `d1<kk>` or `k`),
  `_`, then one or more lowercase hex digits. Those strings name ids and prefixes in revisions.

**Revision expressions.** Parsed into a syntax tree by `bee-core`; resolved by the server.
Violations are `INVALID_ARGUMENT`/`INVALID_REVISION`.

```text
revision := base suffix*                       at most 4096 bytes
base     := "git:" 7..64 lowercase hex          git commit id or abbreviation
          | "k_" 32 lowercase hex               ChangeId: its changeset on the queried ref
          | "c1_" 64 lowercase hex              changeset id
          | "c1_" 10..63 lowercase hex          changeset id prefix (§3.1)
          | refname "@" version                 ref target at that version (>= 1)
          | refname                             ref's current target
suffix   := "~" [n]                             n-th first-parent ancestor (default 1)
          | "^" [n]                             n-th parent, 1-based (default 1; "^0" is itself)
```

Numbers are decimal without leading zeros; suffix numbers fit 32 bits. Ids of kinds other than
changeset are not revisions.

---

## 6. Storage

### 6.1 Two stores

- **Metadata store:** a serializable SQL database. Postgres in production; SQLite in dev mode.
  Each store method is exactly one transaction.
- **Blob store:** immutable, content-addressed bytes in S3-compatible storage or on local disk.

Objects are stored **once per org** (content-addressed dedup within an org); visibility is
always decided per repo (§9.3). Objects never move between orgs.

- **Schema.** The logical tables are `orgs`, `repos`, `objects` (per org: size, state, inline
  bytes), `object_uploads` (org, object, principal), `changesets` (per repo, canonical bytes;
  index on `(repo, change_id)`), `parent_edges` (child, index → parent; index on parent),
  `object_paths`, `derived`, `refs`, `ref_log`, `landed`, `event_counters`, `events`,
  `redactions`, `redaction_epochs` and `gc_marks`. The authoritative listing with keys is the
  `bee_store::types` module documentation.
- **Changesets live only in their `changesets` row**, never in the blob store or `objects`.
- **Object states:** `live` (served); `quarantined` (bytes moved aside, not served; a verified
  re-upload makes the object live again); `purged` (bytes deleted by redaction, §13; the row
  stays as a tombstone). Only redaction moves an object to `purged`.
- **Blob keys:** `o/<org ULID>/<kind tag>/<first 2 hex>/<64 hex>`, and the same under `q/` for
  quarantined bytes. A repeated put refreshes the bytes' write time.
- **Transaction retries.** A store retries its own serialization failures up to 10 times with
  full-jitter exponential backoff (2 ms doubling to 200 ms), then returns `UNAVAILABLE`
  (retryable).

### 6.2 Write ordering

The ordering guarantees that no reader ever sees a reference to bytes that are not durable.

1. **Upload:** write bytes to the blob store, then insert the `objects` row (insert-or-ignore).
   A visible row implies durable bytes. Bytes without a row are orphans that GC removes.
   Trees and metas up to 64 KiB are stored inline in their row.
2. **CreateChangeset:** every referenced object MUST already have a row, or get it in the same
   transaction. In one transaction, insert the changeset row, its parent edges, its
   `object_paths` rows (§9.3) and its eager derived rows (§8), all insert-or-ignore.
   Re-running after a crash repairs missing rows. The objects the changeset itself creates
   (its meta and new trees) MAY be inserted in that transaction, after their blob-store bytes
   (step 1) are durable: they then become visible together with the changeset, never apart.
3. **Ref update:** one transaction that locks the ref row, checks the expected version (or
   absence), writes `version + 1`, appends the ref-log row and inserts `landed` rows. A unique
   violation on `landed` aborts the whole transaction and is reported as a conflict.
   (`ALREADY_LANDED` with `change_id` and `existing_source` in the payload; a version mismatch is
   `VERSION_MISMATCH` with `current_version`.) One update MAY carry K steps, writing versions
   `v+1 … v+K` and K ref-log entries in the same transaction (the land coalescer, §7.4).

"Every referenced object" in step 2 means the parents (changeset rows in the repo), the root
tree, the meta, every non-foreign delta put and every object named by the changeset's
`object_paths` rows; none may be quarantined. A changeset contributes these `object_paths`
rows: its root tree at the root, each delta put's object at its path, and the tree at every
ancestor directory of every delta path that still exists in its root. The union over a repo's
changesets then covers every `(object, path)` reachable in any of them, which fsck checks.

Deletion runs in the opposite order: a row stops being live (purged by redaction, deleted by
GC) before its bytes are deleted. A crash at any step therefore leaves orphans or drafts, never
a dangling reference.

### 6.3 Consistency

- Refs, policy epochs, lease state and redactions are always read with strong consistency.
- Immutable rows (objects, changesets, derived data) MAY be read from replicas, but a miss MUST
  be retried against the primary before returning `NOT_FOUND`.

**Read-your-writes per session.** Every write response carries the version it produced (ref
version, or the repo event sequence number). A client passes the highest version it has seen as
`min_version` on later reads. A replica or cache that has not reached that version waits up to
`read_wait_ms` (default 50 ms) and otherwise forwards the read to the primary. No cross-region
locking is involved, and a client that does not send `min_version` gets ordinary replica reads.

### 6.4 Retention and GC

- Unreferenced changesets ("drafts") are kept for 30 days (repo-configurable) after their last
  use, then become eligible for GC along with objects no longer reachable from any retained
  changeset in the org.
- GC is mark-and-sweep in the background and MUST NOT block writes. An object uploaded in the
  last 24 hours is never collected.
- **Roots** are ref targets, every changeset a ref log ever pointed at (so `name@version` keeps
  resolving), drafts used within the retention window, and forge-supplied roots (open change
  requests). "Use" means created, used as a parent, targeted by a ref or explicitly touched.
- **Mark, then sweep without locks.** GC records a mark, computes what is unreachable, and
  deletes it in one transaction only if no candidate was touched after the mark (re-inserted,
  used as a parent, made a ref target, re-uploaded, or referenced by a new `object_paths` row);
  otherwise it deletes nothing and the next run retries. Rows are deleted before bytes, and
  bytes only if last written before the mark, so a concurrent re-upload survives. Blob-store
  bytes with no row are deleted once older than the upload window. A dry run computes the same
  plan without recording a mark or deleting anything.

### 6.5 Hot and cold storage tiers

Objects not read for `cold_after_days` (repo setting, default 90) move to a cheaper storage class
in the background. Ids, keys and every API stay the same; a read of a cold object is served
transparently, and the node daemon and regional cache keep frequently used cold objects in front of
the slower class. Moving an object between tiers never deletes it from the hot class until the cold
copy is durable and verified against its id.

### 6.6 SQL schema (appendix)

`bee-store-sql` implements the metadata store on Postgres 15+ and SQLite with one versioned
migration set, applied at open and recorded in `bee_schema (version, name, digest)`; a store
refuses a database whose recorded history differs from its own or was migrated by a newer binary.
Every key starts with `org_key`, then `repo_key` (64-bit surrogate keys allocated by the database;
the ULIDs of §3.2 are unique columns of `orgs` and `repos`). Every integer is 64-bit. Ids, paths and
payloads are bytes; text that is ordered (ref names, principals) compares bytewise. There are no
foreign keys: deletion runs in reverse write order (§6.2) and fsck checks references.

| Table | Primary key | Other columns | Secondary indexes |
|---|---|---|---|
| `orgs` | `org_key` | `id` (unique), `name` (unique), `created_at_ms`, `gc_seq` | |
| `repos` | `repo_key` | `org_key`, `id` (unique), `name`, `created_at_ms`, `draft_retention_days` | unique `(org_key, name)` |
| `objects` | `(org_key, object)` | `size`, `state` (0 live, 1 quarantined, 2 purged), `inline_bytes` (trees and metas ≤ 64 KiB), `created_at_ms`, `touched_gen` | |
| `object_uploads` | `(org_key, object, principal)` | `repo_key`, `at_ms` | `(org_key, at_ms)` |
| `changesets` | `(org_key, repo_key, id)` | `change_id`, `bytes` (canonical encoding), `created_at_ms`, `last_used_ms`, `touched_gen` | `(org_key, repo_key, change_id, id)` |
| `parent_edges` | `(org_key, repo_key, child, idx)` | `parent` | `(org_key, repo_key, parent, child)` |
| `object_paths` | `(org_key, repo_key, object, path)` | (`path` empty for the root) | `(org_key, object, repo_key)` |
| `derived` | `(org_key, repo_key, dkey, type_name, version)` | `value` | |
| `refs` | `(org_key, repo_key, name)` | `target` (null when deleted), `version` | `(org_key, repo_key, target)` |
| `ref_log` | `(org_key, repo_key, name, version)` | `old_target`, `new_target`, `principal`, `at_ms`, `landed` (84-byte `change_id ‖ source ‖ result` records) | |
| `landed` | `(org_key, repo_key, ref_name, change_id)` | `source`, `result`, `version` | |
| `event_counters` | `(org_key, repo_key)` | `allocated` | |
| `events` | `(org_key, repo_key, seq)` | `kind`, `payload`, `at_ms` | |
| `redactions` | `(org_key, object)` | `principal`, `reason`, `at_ms` | |
| `redaction_epochs` | `(org_key, repo_key)` | `epoch` | |
| `gc_marks` | `(org_key, seq)` | `at_ms` | |

Concurrency on Postgres: every write that can touch a GC candidate first takes `FOR KEY SHARE` on
its `orgs` row and stamps the rows it touches with the org's `gc_seq`; `gc_begin`, `gc_sweep` and
`redact` take `FOR UPDATE` on that row, so a mark or sweep never interleaves with such a write and
a sweep defers when any candidate's `touched_gen` is at or after its mark. Ref updates run
SERIALIZABLE and lock the ref row `FOR UPDATE` before the version check. Content-addressed rows are
inserted with `ON CONFLICT DO NOTHING`, batched with `unnest` arrays. SQLite runs every write on
one connection with `BEGIN IMMEDIATE` (WAL, `synchronous = FULL`, `fullfsync` on macOS). A
replica read with `min_version` checks the replica for that ref-log row or event row, waits up to
`read_wait_ms`, then reads the primary.

---

## 7. Write path

All writes are server-side and pure where possible: the same inputs produce the same ids, so
any retry is safe.

### 7.1 UploadObjects

Streams objects in batches. The server verifies each object against its id before storing it.

### 7.2 CreateChangeset

Input: parent ids, a delta, meta fields, ChangeId, optional provenance. The server builds new
trees from the parent's root and the delta (clients never send trees), computes the root id,
validates invariant D, stores the result per §6.2 and returns the changeset id.

- A put of a blob the caller neither uploaded nor can read at some path MUST fail with the
  same `MISSING_OBJECT` error as a truly absent blob. Otherwise writing a known id at a
  writable path would make the blob readable.
- `creator` is set from the authenticated principal.

### 7.3 Rebase and three-way merge

**Rebase(C, onto X)** for a single-parent C with base B = tree(parent(C)), ours O = tree(X),
theirs T = tree(C):

- Examine only the paths in C's delta and their ancestor directories. Rebasing across a tip
  that changed other paths reads no blobs.
- If X is already C's parent, return C unchanged.
- The result keeps C's meta and ChangeId and uses no wall-clock time, so it is deterministic.
- A result with an empty delta fails with `EMPTY_CHANGE` (unless `allow_empty`).

**Per-path rules** (b, o, t are the three sides; absent is a value):

1. o = b → take t. 2. t = b or o = t → take o.
3. b absent, o ≠ t → conflict `add_add`.
4. one side deleted, the other modified → `modify_delete`.
5. any side a symlink and sides differ → `symlink`.
6. any side binary (NUL in first 8000 bytes) or larger than `merge_max_text_bytes` (8 MiB) → `binary`.
7. any needed side redacted → `redacted`.
8. otherwise line merge (§7.3.1); modes merge by rules 1–2, else `mode` conflict.
9. structural: a file where the other side has a directory, or a delete under a directory the
   other side kept → `file_directory`.

#### 7.3.1 Line merge (`diff3-v1`)

- Split on `\n`, keeping terminators. No CRLF normalization.
- Myers diff, forward greedy; ties prefer deletion; deletions before insertions in a hunk.
- Hunks whose base ranges overlap or touch form one region. A region changed on both sides
  conflicts unless both sides produce identical lines.
- The exact algorithm, with worked examples, lives in `docs/contracts/diff3-v1.md` and is
  frozen; any change is a new algorithm id.

#### 7.3.2 Conflicts are data

The server never writes conflict markers. A conflict response carries
`{path, kind, base?, ours?, theirs?, hunks?}` for each conflicting path, plus the resolved
entries for every clean path. Clients decide how to present it (markers in a working copy, a
structured payload to an agent, side-by-side in the web UI). A **resolver hook** (§11.6) can
hand the payload to an agent and submit its result as a new changeset.

For a conflicting `Rebase(C, onto X)`, the response message identifies X and C's ChangeId. A
client resolves it by creating a changeset with X as parent and C's ChangeId, then landing that
resolved changeset.

**Merge(A, B)** uses the unique best merge base when there is one. Several best bases
(criss-cross) are merged into a virtual base as `docs/contracts/diff3-v1.md` specifies; no base
uses the empty tree. Renames and copies are detected (exact content first, then line-set
similarity ≥ 0.5) and followed, per the same contract.

### 7.4 Land

`Land(ref, head, expected_version?)` puts the linear stack ending at `head` onto `ref` without
a global lock.

1. Read the ref tip and version strongly. Evaluate policy (§9) for this principal, ref and
   stack on every attempt.
2. The stack is the first-parent chain from `head` back to the first ancestor of the tip.
   Merge changesets may be members: a merge member is rebased by replaying its delta from its
   first parent onto the running tip and keeps its other parents, so the side branches it
   merged land with it. A land rule with `require_linear` refuses a stack containing a merge
   (`POLICY_DENIED`, requirement `linear`). The single-change `Rebase` RPC still takes
   single-parent changesets only, and the `MERGE_COMMIT` land method stays refused
   (`MERGE_IN_STACK`).
3. For each member, look up `landed[(repo, ref, change_id)]`: same source changeset → skip;
   different source → `ALREADY_LANDED`; absent → rebase onto the running tip.
   A member that rebases to nothing is `EMPTY_CHANGE` unless the land sets `allow_empty`;
   then no changeset is written for it and it is recorded in the landed index with the running
   tip as its result (the next member rebases onto that tip), so its retry is idempotent too.
4. If every member was skipped, return the recorded result. This makes a retry after
   "committed, response lost" idempotent.
5. Otherwise CAS the ref (§6.2 step 3). On version mismatch or a `landed` violation, retry from
   step 1 with full-jitter backoff, up to the repo's attempt budget.

**Attempts and states.** The attempt budget is `land.attempt_budget` CAS attempts per land
(default 16; one coalesced batch counts once for all its stacks), with full-jitter backoff
starting at 2 ms and doubling to a 200 ms cap. `LandResponse.attempts` reports how many a land
took. Every land ends in exactly one named state, counted in `bee_lands_total{result}`:
`landed` (published), `skipped` (every member was already landed from the same source),
`conflict` (`MERGE_CONFLICT`), `denied` (`POLICY_DENIED` or `LEASED`), `contention` (the budget
ran out: `LAND_CONTENTION`, `UNAVAILABLE`, retryable; nothing was published) or `error` (any
other failure). While running, a land is `queued` (waiting for its coalescer), `planning`
(stack walk, policy, rebase onto the running tip) or `publishing` (in the batch's CAS).

**Land coalescer (required).** Every protected ref has exactly one owner task at a time, held by a
lease row whose version doubles as a fencing token. All lands for that ref are routed to its owner,
which takes every validated land that is waiting (up to a configured batch limit), rebases the
queued stacks onto each other in arrival order in memory, and publishes them with one CAS that
writes version + K, K ref-log entries and the landed rows. Each stack's result and ref-log entry are
the same as if it had landed alone at that position. The API does not change. The rebased stacks'
trees and changesets are written together in one transaction before the CAS, not one or two per
stack. A land forwarded to the owner names the owner's process and the fencing token the forwarder
read; any other process (for example one that took a crashed owner's address) refuses it, and the
forwarder lands it itself and takes the lease over.

If publication of a batch fails without a retry, every land planned in that batch receives the
retryable publication error; no caller receives an outcome computed from the unpublished batch.

Measured basis (bench/db, 2026-10-18, one laptop, single-node databases): with one transaction per
land every candidate database stayed between 38 and 438 lands a second on one ref; with a
coalescer, Postgres reached 81,318 lands a second at 1,000 concurrent landers (p99 41.8 ms).

**Hot counters.** The per-repo event sequence (§11.7) is another single hot row; sequence numbers
are allocated in blocks or by the same owner task, never one transaction per event.

### 7.5 UpdateRef

Plain CAS for non-protected refs (`ws/*`, `green/*`): `{name, expected_version | absent, new_target}`.

---

## 8. Derived data

A derived type is `{name, version, mode: eager|lazy, key(changeset or object) → key, derive}`.
`derive` MUST be deterministic. Values are written insert-or-ignore, so concurrent
derivations are harmless; leases only avoid duplicated work.

- **Eager** types are computed in CreateChangeset's transaction.
- **Lazy** types find the nearest ancestor with a stored value and derive forward, writing
  back as they go.
- Bumping a type's `version` re-derives on demand with no client change.

| Type | Mode | Contents |
|---|---|---|
| `graph` | eager | generation number; skip pointers to first-parent ancestors at distance 2^k; count of merges on the first-parent chain (equal counts mark a merge-free span skip pointers can cross). Gives O(log n) ancestry and merge base on linear history. |
| `path_history` | eager | per-path history nodes, reused when content is unchanged from parent 0 |
| `object_paths` | eager | (repo, object, path) rows used for authorization (§9.3) |
| `blame` | lazy | line ranges per file-history node |
| `diff_class` | lazy | semantic classification of a changeset's delta (§11.3) |
| `git_mapping` | importer | `git sha ↔ changeset id`, trees and blobs too: v1 keys a changeset, tree or blob id to the raw git id (20 or 32 bytes), and the git id's foreign id (`g2_…`, §3.1) to the 34-byte bee id; written in the changeset's CreateChangeset transaction |
| `git_tag` | importer | v1 keys an annotated tag's foreign id to the tag object's exact bytes; the `tags/*` ref targets the peeled commit's changeset |
| `copy_detection` | lazy | where each added file came from: exact (by blob, via `object_paths`) or similar (line sets ≥ 1/2) copies and renames |
| `language_stats` | lazy | files and bytes per language, linguist-style, `.gitattributes` `linguist-*` overrides; forward from parent 0 |
| `git_mapping` | importer | `git sha ↔ changeset id` |

Row formats (sCBOR maps, `bee-history` and `bee-derived` document each field):

- `graph` v2, keyed by changeset: `{1: generation, 2: first-parent depth, 3: merges on the
  first-parent chain, 4: [parents], 5: [skip ids, skips[k] at distance 2^k]}`. v1 was the test
  fixtures' generation-only stand-in.
- `path_history` v1: keyed by changeset `{1: root node id}`; keyed by node id (derived kind
  `0x10`, BLAKE3 of the node encoding) `{1: kind (0 file, 1 dir, 2 deleted file, 3 deleted
  dir), 2: path, 3: linknode changeset, 4: its generation, 5: [previous nodes], 6: entry,
  7: [{name, node, kind}]}`. Deleted paths stay as tombstones, so history continues through
  delete and re-add; a node is reused when a parent (parent 0 first) has the same state; an
  added file whose blob the same delta deletes elsewhere also links to that node (exact
  rename).
- `blame` v1, keyed by file node (or derived kind `0x11` for an ignore list):
  `{1: [[length, origin node, origin line]]}`.
- `git_mapping` v1: changeset → raw git id; foreign id (`g2_…`) → changeset id bytes.

Lazy derivation walks dependencies with an explicit stack, never recursion.

**Generic API.** `ListDerivedTypes` names every registered type with its version, mode
(`eager`, `lazy`, `importer`), key kind (`changeset`, or `history_node` for per-path types)
and content-bearing flag (§13). `GetDerived(type, key, version?, path?, derive_if_missing)`
returns the stored value of the current (or the given) version; with `derive_if_missing` a
missing lazy value is derived (forward from the nearest stored ancestor) and written back, and
`derived_now` says so. Without it, or for an importer type with no row, an absent value is
`NOT_FOUND`, as is an unknown type. A per-path type also takes a changeset key and a `path`:
the server resolves the file history node at that path (`NOT_FOUND` if absent) after checking
read access to it. Content-bearing types need read access at `/`. Nothing about a new type
needs a new RPC, and a version bump re-derives with no client change.

---

## 9. Identity, authorization and policy

### 9.1 Principals

A principal is `{id (ULID), kind: human | agent | service, org_id?, display, external_ids[]}`.

- **Humans** log in with GitHub device flow or OIDC. External ids are stable provider ids
  (`github:<numeric id>`, `oidc:<issuer>#<sub>`), never logins, which can be recycled. The server
  runs GitHub's device flow (`Login{github_start}`, then `Login{github_device_code}` until the
  human approves) or exchanges a GitHub token (`Login{github_token}`, proven with `GET /user`);
  an id_token from the configured OIDC issuer is verified against its JWKS (signature, `iss`,
  `aud`, `exp`, `nbf`, `iat`). A GitHub login replaces the principal's external groups with its
  GitHub teams (`<org>/<team>`); an OIDC login with the id_token's groups claim. When GitHub is
  unreachable new GitHub logins fail with retryable `UNAVAILABLE`; issued tokens are verified
  locally and keep working, with the groups synced at their login.
- **Beehive is an OIDC provider**: OAuth grants with the `openid` scope also return an ES256
  `id_token` (`sub` = the human), with discovery, JWKS and userinfo endpoints.
- **Agents** are created by a human, a service or another agent and always have an **owner**:
  the principal that created it. A human or service creating an agent uses its own recorded org
  when the request leaves it empty or names that recorded org; otherwise it must name an existing
  org it belongs to. A server administrator may name any org, including one without a forge
  record; resolved org names are stored canonically. An agent principal records `{owner,
  runtime (e.g. "claude-code", "codex"), purpose}`. An agent creates sub-agents only in its own org (the
  request's org is empty or names it; otherwise `PERMISSION_DENIED`), and only while its own
  owner chain (the agent, its owner, theirs, back to a human or service) is shorter than seven,
  so no chain grows past seven (`PERMISSION_DENIED`). Tokens for a sub-agent are minted by its
  owner, attenuated from the owner's own token (§9.2), so they never exceed it.
- **Services** (CI, the importer, bots) authenticate with a secret or workload identity.

### 9.2 Delegated tokens

Tokens are **Biscuit** tokens (public-key signed, offline-attenuable). Any holder can derive
a narrower token without contacting the server; the server checks the whole chain.

A token's authority is the intersection of every block in its chain:

- `principal`: who is acting; `on_behalf_of`: the chain of delegators back to a human or service.
- `repos`, `paths` (read/write prefixes), `refs`, `actions` (e.g. `read`, `commit`, `land`,
  `review`, `lease`).
- `expires_at`; `budget` (max changesets, max landed changes, max bytes uploaded).
- `session`: an opaque agent session id copied into provenance.

Revoking any token id revokes every token derived from it. Revocations propagate through the
event stream and the policy epoch (§9.5).

Token rules:

- **Blocks.** Every block carries its scope twice: as Datalog `reject if` / `check if` rules over
  facts the verifier supplies (`time`, `repo`, `operation`, `read_path`, `write_path`, `ref`), so
  any Biscuit verifier enforces it; and as JSON in the block's signed `context`
  (`{"v":1,"kind":"authority"|"attenuation",…,"scope":{…}}`), from which the server computes the
  effective scope and charges budgets. A request must pass both. A block without a valid context
  is rejected, so no restriction can be hidden from the effective scope.
- **Principal chain.** Only the authority block names the principal and `on_behalf_of` (nearest
  delegator first). An attenuation block may name a sub-agent in `act_as` (agents only); the
  current principal then moves to the front of `on_behalf_of`. An agent token is minted on
  behalf of the agent's owner. The last entry of `on_behalf_of` is the **delegation root**.
- **Dimensions.** An unset dimension inherits; an empty list allows nothing. Repos and refs are
  patterns (exact, or a prefix with one trailing `*`); paths are UTF-8, component-wise prefixes
  (`docs` covers `docs/a`, not `docs2`; `/` is the whole repo). Expiry is the earliest of all
  blocks; the session is the innermost block's.
- **Repo names.** Repo patterns match the repo's full name (`org/name`). A token that lists a
  repo's id as an exact entry also covers that repo; patterns are never matched against ids, so
  no prefix (`01*`) reaches a repo through its id. One rule, on every surface (core and forge
  RPCs, path checks, git smart HTTP and SSH): the request is checked with the full name first,
  and with the id only when the token's effective `repos` lists that exact id; a refusal names
  the full name. `MintToken` keeps an id in `repos` as given, and `WhoAmI` and `ListTokens`
  report it unchanged.
- **Lifetime.** 12 hours from minting unless the mint request sets `expires_at`. A token is
  valid while `now < expires_at`; attenuation cannot extend it.
- **Ids and revocation.** A token's id is the hex revocation id of its last block. A derived
  token contains every block of its parent, so revoking a token id revokes it and everything
  derived from it, and leaves its parent and siblings valid. Each new revocation increments the
  repo's revocation counter (§9.5).
- **Signing keys.** Each root key has a `u32` key id, written into every token it signs. The
  first key of the key ring signs; every listed key verifies. Rotation puts a new key first and
  retires the old one after its tokens have expired. A token without a key id, or with an
  unknown one, is rejected.
- **Budgets and quotas.** A request is charged to every budgeted block of its token (keyed by
  the block's revocation id), so a sub-agent's usage also counts against each ancestor's budget,
  and to the quotas of the principal and of the delegation root (§15.4). A charge is atomic: all
  keys or none.
- **Service secrets** are stored only as SHA-256, compared in constant time and never logged; an
  unknown service name costs the same comparison.
- **Reasons.** `UNAUTHENTICATED` (malformed token, bad signature, unknown key id, invalid block,
  wrong secret), `TOKEN_EXPIRED`, `TOKEN_REVOKED`, `POLICY_DENIED` (outside the token's scope),
  `BUDGET_EXHAUSTED`, `INVALID_ARGUMENT` (bad mint or attenuation request). None is retryable.

### 9.3 Path authorization

Objects carry no paths, so read access goes through the `object_paths` index:

- An object is readable **through repo R** if some `(R, object, path)` row exists with `path`
  readable by the principal in R. Policy changes apply retroactively with no reindexing.
- Every object RPC is repo-scoped. Dedup across repos (§6.1) never makes an object readable
  through a repo that does not reference it.
- An uploaded but never-referenced object is readable only by its uploader, until GC.
- `HasObjects` answers false for objects the caller cannot read, so existence cannot be probed.
- Trees keep listing restricted entries (name and id) inside readable parents; only fetching
  the restricted subtree is denied, so hashes never change.
- Responses that drop unreadable delta entries are marked `filtered = true`; a filtered view is
  not the hashed encoding, and clients MUST NOT verify ids against it.

### 9.4 Policy

Policy is code, versioned in the repo itself at `.beehive/policy.toml`, plus org-level defaults
stored server-side in the same format. The effective policy is org defaults overlaid by the repo
file at the **tip of the target ref** (a change to policy takes effect only once it lands, and
landing it is itself governed by the old policy).

**Invalid policy.** Parsing returns a typed error naming the input (org default, repo file or
`CODEOWNERS`), the TOML key path, and the line and column; clients see `INVALID_POLICY`. The
evaluator never falls back by itself: keeping the last good effective policy is the caller's
job. The server validates `.beehive/policy.toml` (and the `CODEOWNERS` file it names) at land and
refuses a land that would make the policy invalid; if an invalid policy is found at a tip anyway,
the server keeps serving the last good policy for that ref. An invalid org default is fatal at
startup.

**Format.** Unknown keys are errors. Paths may be written with or without a leading `/`; `/` is
the root. Subjects are principal ids or `@group`.

```toml
version = 1                          # required
codeowners = "CODEOWNERS"            # optional CODEOWNERS file consulted for owners

[groups]                             # name -> members: principal ids and @nested groups
core = ["01J0ALICE", "@reviewers"]
reviewers = ["01J0BOB"]

[roles.releaser]                     # custom role: a base role plus actions
base = "write"
actions = ["manage_refs"]

[members]                            # role -> subjects holding it
admin = ["@admins"]
write = ["@core", "@agents"]
read = ["@everyone"]

[[access]]                           # read and write rules by path prefix
path = "/"                           # required, setting both read and write
read = ["@everyone"]
write = ["@core", "@agents"]

[[access]]
path = "/secrets"
read = ["@security"]
write = ["@security"]

[[refs]]                             # who may create, update, delete, force-update refs
pattern = "release/*"
create = ["@releasers"]
delete = []

[[land]]                             # requirements to land on matching refs
ref = "main"
allow = ["@core", "@agents"]         # optional: who may land
required_checks = ["test"]
require_signed = false
require_linear = true
dismiss_stale_approvals = true
require_last_push_approval = false
agents_may_land_without_human = ["docs", "test_only", "comment_only"]
require_agent_review = false         # an agent must review (not request changes)

[[land.code_scanning]]               # open alerts of a tool block the land
tool = "codeql"                      # compared case-insensitively
alerts = "errors"                    # none, errors (default), errors_and_warnings, all
security_alerts = "high_or_higher"   # none, critical, high_or_higher (default), medium_or_higher, all

[[land.approvals]]
classes = ["logic"]                  # empty: every class
paths = ["/src"]                     # empty: every path
count = 1                            # default 1
from = "owners"                      # "owners", "any" (default) or ["@group", "id"]
include_agents = false               # whether agent and service approvals count

[[owners]]
path = "/src/auth"
owners = ["@security"]

[push]                               # checked for every added or modified file
deny_paths = ["**/*.pem"]
deny_extensions = ["exe"]
max_file_size = 104857600
max_path_length = 1024

[paths]
sensitive = ["/.bee", "/src/auth", "/.github/workflows"]
generated = ["**/Cargo.lock"]
exclusive_lease = ["/schema"]

[[rulesets]]                         # also takes every [[land]] requirement key,
name = "two-humans"                  # [[rulesets.approvals]] and [rulesets.push]
mode = "evaluate"                    # active (default), evaluate or disabled
refs = ["main", "release/*"]
bypass = ["@admins"]

[rulesets.repo_properties]           # optional: only repos whose custom properties match
environment = ["production"]         # every key must hold one of its values
```

**Semantics.**

- **Groups.** Built in: `everyone` (every authenticated principal), `humans`, `agents`,
  `services`; they cannot be redefined. A group name containing `/` (`@org/team`) is an external
  group supplied by the identity provider (forge team membership, GitHub team sync, OIDC groups)
  and needs no definition; any other `@name` must be
  defined. Groups nest; a cycle is an error that names it.
- **Repo roles as groups.** On a repo the forge manages (§9.6), every principal the policy
  evaluates for a caller (the repo gate, path checks, ref checks, land evaluation and the
  forge's own policy checks) carries the external group `@repo-role/<role>`, where `<role>` is
  that principal's **own** effective forge repo role on the repo: `read`, `triage`, `write`,
  `maintain` or `admin`; a principal with no role gets no such group. There is no agent
  inheritance here (repo access in §9.6 keeps it): the group is the policy's input, and policy
  narrows agents, so an agent owned by an org admin holds only what its own role gives it.
  On a repo the forge keeps no record of, no principal carries the group. Only the server
  computes these groups, per repo; a `repo-role/…` group from any other source (an identity
  provider, a principal's stored groups, a forge team) is dropped. Org-level checks (org
  policy, exports, imports, org rulesets) use no repo-role group, and neither do reviewers
  whose approvals a land rule counts: only the caller's chain carries it.
- **Built-in org default.** Used when no org default file is configured:
  `admin = [<org admins>…, "@repo-role/admin"]`, `maintain = ["@repo-role/maintain"]`,
  `write = ["@humans", "@agents", "@services"]`, `read = ["@everyone"]`, the `/` rule reading
  and writing for `@everyone`, and land rules on `main` and `release/*` with no checks or
  approvals and agents allowed to land every class. A repo `[members]` entry replaces the org
  entry of the same role (Overlay below), so a repo file that sets `admin` decides who is
  admin there, and `@repo-role/admin` holds `admin` only if it lists it; roles the repo file
  does not set keep the org entry. An operator's own `policy.org_default_file` may name the
  same groups.
- **Roles.** Built in, each including the one before: `read` (read, issues_write, swarm_join); `triage` (+
  triage); `write` (+ review, commit, lease, land); `maintain` (+ manage_refs, manage_settings);
  `admin` (+ force_update, redact, purge, manage_policy). `issues_write` opens issues and
  comments on them (and edits or closes one's own); every role has it, so a token's `actions`
  decide whether it may write to issues (a token can be limited to `read` + `issues_write`).
  `swarm_join` governs admission to a swarm under repository policy (§11.10).
  Denials of state-changing actions on the VCS path are appended to the org's audit log
  (`vcs.<action>`, `result` = the reason). A custom role extends a base role (built-in or
  custom; cycles are errors) with more actions. A principal's actions are the union over every
  role `[members]` gives it.
- **Access.** `check_access(principal, groups, action, path)`: the principal's roles must grant
  the action. Then `read` and `review` need read access and `commit`, `lease` and `land` need
  write access, decided by the longest **component-wise** prefix rule that sets `read`
  (respectively `write`): `/secrets` covers `secrets/x` but not `secrets2`. Read and write rules
  are matched independently. Other actions are repo-wide. The required `/` rule makes the
  default explicit. For a delegated call every principal in the chain must be allowed.
- **Ref patterns** are an exact name or a prefix ending in a single `*`. When several match, an
  exact name beats every pattern and the longest pattern wins. A ref matched by a land rule or an
  active ruleset is **protected** and moves only through Land, never by `UpdateRef`. `ws/<id>/…`
  refs are always writable by `<id>` and its delegates. For other refs the most specific
  `[[refs]]` rule decides when it lists the operation; otherwise create, update and delete need
  `manage_refs` and force needs `force_update`.
- **Land.** `evaluate_land` returns `{allowed, satisfied[], missing[], rule, reason, log[]}`. The
  ids in `missing[]` are the strings of `PolicyDenial.missing` (`check:test`,
  `approval:owner:src/auth`, `approval:owner:src/auth:human` for a human-only approval,
  `approval:human`, `leased:src/x`, `push:max_file_size:big.bin`, …);
  `reason` is `LEASED` when a lease conflict is missing, else `POLICY_DENIED`. It checks, in order:
  1. every principal in the chain holds `land` and may write every changed path;
  2. the most specific `[[land]]` rule: `allow` (every principal in the chain), required checks
     (the last result per name must be success, neutral or skipped), signed changesets, a linear
     stack, approvals, the agent rule, the last-push rule, code-scanning thresholds
     (`code_scanning:<tool>`), a required agent review (`review:agent`), the rule's own push
     rules;
  3. leases: a path under another principal's exclusive lease fails with `LEASED`; a path
     matching `paths.exclusive_lease` needs an exclusive lease held by someone in the chain;
  4. the top-level push rules, then every enforced org-required workflow that applies
     (`workflow:<name>` until its latest run on the head succeeded);
  5. every matching ruleset that is not disabled.
- **Signed changesets.** A changeset is signed when its Meta carries a git commit with a
  `gpgsig` header (imports and git pushes); the signature (OpenPGP or SSH) must verify against a
  signing key registered on an account (the account is the signer). Changesets written natively,
  including the rebased or squashed results of a land, are unsigned, so `require_signed` is
  evaluated on the stack as submitted.
- **Code scanning.** An open alert counts when it has an open instance analysed at the change
  request's head or the head being landed, or on the target ref; it blocks when its level meets
  `alerts` or its security severity meets `security_alerts`.
- **Agent review.** `require_agent_review` needs a review by an agent principal that does not
  request changes (of the head revision under `dismiss_stale_approvals`); while some matching
  rule requires one, the repo's configured review agent is asked to review every opened change
  request and new revision.
- **Repo properties.** A ruleset with `repo_properties` applies only to repos whose custom
  property values (org defaults filled in, multi-select values one by one, plus `visibility`)
  match; the effective policy of other repos leaves it out (and so has its own epoch).
- **Approvals.** A reviewer's last review counts. The acting principal's own reviews never count;
  with `dismiss_stale_approvals` only reviews of the head revision count; the reviewer must hold
  `review`. An outstanding request for changes is a missing requirement. An approval rule applies
  when some changed path has one of its classes and lies under one of its paths, and needs
  `count` distinct approvers from `from` (humans only unless `include_agents`). With
  `from = "owners"` the matched paths are grouped by owner set and each group needs `count`
  approvals from its owners; unowned paths accept any approver. A path matching
  `paths.sensitive` is `sensitive` whatever the classifier said.
- **Agents.** When the acting principal is an agent and some changed path's class is not in
  `agents_may_land_without_human`, at least one human approval is required; an unset list means
  agents always need one.
- **Last push.** With `require_last_push_approval`, someone other than the last pusher and the
  author must approve the head revision.
- **Owners** of a path: the longest `[[owners]]` prefix, otherwise the last matching line of the
  `CODEOWNERS` file (GitHub syntax: a leading or inner `/` anchors, a trailing `/` matches
  directories only, `*`, `?` and `**` as in globs; `!` and `[ ]` are errors). A `CODEOWNERS`
  `@login` or email matches a principal's aliases; `@org/team` matches an external group.
- **Globs** (push rules and `[paths]`): `**` matches any number of components, `*` and `?` match
  within one component, patterns are anchored at the root, and a glob that matches a directory
  covers everything under it (`/schema` covers `schema/a.sql`).
- **Push rules:** denied globs, denied extensions (case-insensitive; `tar.gz` style allowed), and
  maximum file size and path length in bytes.
- **Rulesets.** Every matching ruleset applies on top of the land rule. `active` rulesets enforce
  their requirements; `evaluate` rulesets are evaluated and written to the decision log with
  `would_block`, but never enforced; `disabled` rulesets are skipped. If every principal in the
  chain is a bypass actor, the ruleset is logged as `bypassed` and not enforced.
- **API rulesets.** A ruleset created through `PolicyService` is rendered as a `[[rulesets]]`
  entry named `api-<id>` whose `agents_may_land_without_human` lists every class, so it adds no
  implicit human approval. Humans are required only by a required-approvals rule's
  `human_count`: when it is above zero the rule renders an approval of `human_count` from humans
  only, plus, when `count` is larger, a second approval of `count` in which agents count when
  `agents_may_approve`; when it is zero the rule renders one approval of `max(count, 1)` in which
  agents count when `agents_may_approve`. Both approvals take the rule's classes, paths and
  approver set, and each is enforced on its own.
  API rulesets may also carry a `merge_queue` rule, which the forge enforces rather than the
  land evaluator: while an `active` matching ruleset has it, the target ref accepts publication
  only from a merge-queue entry in `LANDING` for the matching head. This gate is at publication
  for every route (native `Land`, change requests and git push); a queue-required branch is a
  protected ref for direct ref writes and git push, has no bypass, and otherwise
  refuses with `INVALID_STATE` / `merge_queue_required`, exactly as with the repository setting
  `merge_queue_required` (`docs/spec/operations.md` §8, `docs/spec/forge-changes.md`).
  During a forge database outage, ordinary branches continue to land; a branch required by an
  active API ruleset or a remembered repository setting remains queue-gated until the forge recovers.
  `max_entries_to_merge` is an enforced per-ref queue capacity; the smallest non-zero maximum
  across matching active rulesets wins, and a full queue returns `RESOURCE_EXHAUSTED` with
  `merge_queue_full`. The one-at-a-time queue fixes `min_entries_to_merge` at one,
  `check_timeout_seconds` at zero, and `all_entries_must_pass` at false. It refuses
  `merge_method`, `max_entries_to_build`, and `min_entries_wait_seconds`, as well as any other
  value for those fixed parameters, with `INVALID_ARGUMENT` / `unsupported_rule` naming the
  field.
- **Overlay.** A repo entry replaces the org entry with the same key (groups, custom roles,
  `[members]` roles, ref rules, land rules, owner rules); access rules for the same path override
  `read` and `write` separately; path sets and push deny lists are unions; push limits and
  `codeowners` are replaced when set; org rulesets always apply, and a repo ruleset with the same
  name is an error. Cross-references (groups, roles, the `/` rule) are validated on the result.

### 9.5 Epochs

Each repo has a **policy epoch**: a hash of its effective policy plus a revocation counter.
Every response carries the epoch in a trailer. Caches (daemon, web) MUST drop authorization
decisions when the epoch changes. Epochs are per repo, so a change in one repo never flushes
another's caches.

- The **policy hash** is the first 8 bytes, big-endian, of BLAKE3 over the canonical form of the
  effective policy, including the attached `CODEOWNERS` entries. The canonical form encodes the
  normalized policy as length-prefixed tokens (`<len>:<bytes>`) after the header
  `swarm-policy-canon-v1`: sections in a fixed order, keyed rules and sets sorted, paths
  normalized. Comments, whitespace, key order, list order, duplicate entries and path spelling
  therefore never change it. The layout is defined in `crates/bee-policy/src/canon.rs` and
  frozen by a golden test; changing it needs a new header.
- The **epoch** is the first 8 bytes, big-endian, of BLAKE3 over `swarm-epoch-v1\0`, the policy
  hash and the revocation counter (8 bytes big-endian each). The `bee-policy-epoch` trailer
  carries it as a decimal `u64`.

### 9.6 Repo access

When the forge runs, its repo record (§11.1) is authoritative for who may reach a repository at
all, on every surface: the VCS RPCs, git smart HTTP and SSH, LFS, the node daemon's checks, signed
URLs, event streams, search, exports and imports, MCP and the GitHub gateway. One decision answers
"may this principal chain do this action on this repo", before token scope (§9.2) and policy
(§9.4) are checked:

- **Lifecycle.** A deleted repo (or one in a deleted org) answers `NOT_FOUND` to everyone, for
  every action, until it is restored; existence is not revealed. An archived repo serves reads to
  those who may read it and refuses every other action with `ARCHIVED`.
- **Roles.** Every principal of the chain needs a repo role (org owner/admin, org base role,
  direct and outside collaborators, teams, app installations, deploy keys, fork access) whose
  built-in actions (§9.4) contain the action. Public repos grant read to every authenticated
  principal, internal repos to org members. An agent acts with its own role or that of the
  principal it acts for, whichever is higher, so the humans and services of the chain decide
  (token scope and policy narrow agents). The roles also reach the policy: each principal of
  the chain is evaluated with `@repo-role/<role>` for its own role, without that inheritance
  (§9.4), and every principal of the chain must still hold the action there, as the token
  must allow it. A caller that cannot read a non-public repo gets `NOT_FOUND`;
  one that can read but lacks the action gets `PERMISSION_DENIED`.
- **Anonymous read.** While `forge.allow_anonymous_public_read` is on (the default), a request
  without credentials to a read RPC (VCS reads, `RepoService.GetRepo`, search, trending, topics,
  leaderboards and public profiles; the reads behind a repo's tabs: context, change requests and
  their reviews, checks, leases, workflow runs, jobs and logs, issues, projects, releases,
  packages, Pages and languages, the repo's posts, and the repo's event stream;
  `docs/spec/server.md` §4.1) or a
  git clone or fetch over HTTP is decided with an empty chain: `Read` on a public repo,
  `NOT_FOUND` for everything else, including repos the forge keeps no record of. What such a
  read returns never names content of a repo the caller cannot read: sub-issues in other repos
  are left out, and project items from them are placeholders that no filter or sort looks at.
  Org membership, agents, principals, tokens and security alerts stay closed. Every other call
  and every write without credentials answers `UNAUTHENTICATED`; git asks for credentials
  instead of answering `NOT_FOUND`. Such requests run in the `anonymous` admission tier (§15.4).
- **Transfer.** A transferred repo keeps its id and its VCS object namespace (like a fork, §11.1),
  so every object stays addressable by the same ids; access, the governing org policy and
  admission follow the new owner at once, and the old full name redirects for 90 days.
- **Propagation.** A change made through a server applies to its next request. Every server
  reuses decision inputs for at most `forge.access_cache_ms` (default 1 s), which bounds a stale
  allow anywhere; node daemons add their confirmation bound (`docs/spec/cache-tiers.md`), and
  signed URLs served by an external store their lifetime (§10.5). URLs the server serves itself
  decide again when redeemed, and open event streams at least every second.
- **Fail closed.** When the forge cannot answer, a decision input fetched in the last
  `forge.access_outage_grace_ms` (default 5 min) is reused; otherwise the call is refused with
  `UNAVAILABLE`. A repo the forge keeps no record of is governed by policy alone.

### 9.7 Operators and administrative scope

A server serves many tenants, so administrative RPCs are scoped by the data they touch:

- **Server-wide** (logs, metrics, traces, config, rollouts, the status page's writes, backups and
  restores, signing keys, cache tiers): only **operators**, principals listed in the server's
  `ops.operators` or `forge.site_admins`. Every principal of the chain must be listed, and the
  token must allow `manage_settings`. Administering an org, any org, never makes a principal an
  operator. A development server lists its dev user explicitly.
- **Org-scoped**: an administrator of the org the request names or that owns its target, through
  that org's policy; administering another org grants nothing.
- **Enterprise-scoped**: the owners of that enterprise.

The classification of every administrative RPC is in `docs/spec/operations.md` §1.1.

---

## 10. Fetch, cache and working copy

### 10.1 Fetch walk

Clients never send "have" lists. To check out: resolve the ref → changeset → root tree; fetch
missing trees breadth-first in batches within the sparse scope (recording denied subtrees);
stream blobs; verify every item against its id before writing it anywhere. Interrupted streams
resume by offset.

### 10.2 Node cache (`bee daemon`)

One daemon per machine serves every job on it over a unix socket and answers the same object
RPCs as the server (tier conformance: identical bytes).

- Writes are temp file → verify → fsync → rename. Concurrent misses for one object coalesce
  into one upstream request. Eviction is approximate LRU between watermarks.
- Cached ref reads have a short TTL and never go backwards in version.
- Authorization: tokens are verified locally; objects whose only paths are readable by every
  repo reader may be served once the principal has one positive repo-read decision; other
  objects need a per-(principal, object) decision cached until the epoch changes. If the daemon
  cannot confirm it is current within a bound, it forwards everything (fail closed).

### 10.3 Working copy

A working copy is a directory with `.bee/` holding: the server endpoint and repo
(`config.json`), the checked-out changeset, stack, sparse profile and rebase in progress as
canonical TOML (`state.toml`), the index (`manifest` and `statcache`: sealed binary files with
a magic, a format version and a BLAKE3 trailer; an unknown version or a torn file is refused),
a pending-operation journal (`journal/`) and log (`oplog.jsonl`), local objects, one file per
conflicted path of a stopped rebase (`conflicts/`), and an advisory `lock` held by the command
running (`docs/spec/client-and-cache.md` §2).

- **Sparse profile:** a list of included path prefixes (empty = whole repo).
- **Status** compares the stat tuple (size, mtime_ns, ctime_ns, inode, mode) with the cache; a
  file modified at or after the cache write time is re-hashed ("racily clean"). Status MUST NOT
  touch the network when the needed trees are cached.
- **Commit** uploads changed blobs and calls CreateChangeset. **Land** calls Land. There is no
  separate push: a commit is on the server when the command returns.
- Every mutating command journals its intent first; the next command completes or rolls it back.
- **Line endings** follow `.gitattributes` (`text`, `-text`, `binary`, `text=auto`,
  `eol=lf`, `eol=crlf`; patterns as in `.gitignore`, deeper files and later lines winning per
  attribute): a text path is stored with LF (CRLF is normalized before hashing, so status,
  commit and diff agree) and written with CRLF only under `eol=crlf`; any other path is stored
  and written byte for byte. There is no client-side `autocrlf` setting, so every client and
  the server agree on blob ids (`crates/bee-wc/src/eol.rs`).
- On case-insensitive filesystems, entries whose names collide under ASCII case folding are
  reported and not written.
- Symlinks are written as symlinks; checkout never follows a pre-existing symlinked parent.
  On unix every working-tree write is relative to directory descriptors opened with
  `O_NOFOLLOW` (`openat`, `mkdirat`, a temporary created with `O_CREAT | O_EXCL | O_NOFOLLOW`,
  then `renameat`), so a symlink swapped in after a check can never redirect a write.

### 10.4 Provenance

The optional `provenance` map in Meta (§4.3) records:

| Key | Field |
|---|---|
| 1 | `principal_chain`: principal ids from the acting agent back to its human or service root |
| 2 | `runtime`: agent runtime and version, e.g. `"claude-code/3.2"` |
| 3 | `model`: model id |
| 4 | `session`: opaque session id (links to an external transcript, never inlined) |
| 5 | `prompt_digest`: BLAKE3 of the task prompt, never the prompt itself |
| 6 | `checks_run`: array of `{name, result, duration_ms}` the agent ran locally |
| 7 | `cost`: `{input_tokens, output_tokens, usd_micros}`, all optional |

The server fills `principal_chain` from the token and rejects a client-supplied chain that
disagrees. Everything else is the client's claim and is shown as such. Because provenance
lives in Meta, it is redactable without changing changeset ids.

### 10.5 CDN-backed object reads

Objects are immutable and content-addressed, so they can be served from a CDN without
invalidation. When a client sets `allow_signed_urls` on GetObjects, the server may answer an object
with a signed URL instead of its bytes, after the same authorization check as a direct read:

- URLs are scoped to one object, expire after `signed_url_ttl` (default 60 s), and are never issued
  for redacted or quarantined objects. URLs the server serves itself carry the repos and the
  principal chain they were issued for, bound to the signature, and repo access (§9.6) is decided
  again when they are redeemed.
- The client verifies the downloaded bytes against the object id exactly as for streamed bytes.
- Redaction purges the object from the CDN and from storage; the redaction propagation bound must be
  at least `signed_url_ttl`, so an unexpired URL cannot outlive the bound.
- The cloud uses this for large objects; self-hosters can point it at any S3-compatible store or
  CDN, or leave it off.

---

## 11. Beehive (forge)

### 11.1 Orgs and repos

- **Org:** `{id, name, display, created_at}`. Members have roles `owner | admin | member`.
- **Repo:** `{id, org_id, name, visibility: private | internal | public, default_ref, settings}`.
  Creating a repo creates its empty root and `main` at version 1.
- Renames change only the name; ids and keys are untouched. Old names redirect for 90 days.
- **Forks** (`docs/spec/forks.md`): a fork's VCS repo lives in the object namespace of its
  network root, so shared objects are stored once and never copied; forking copies changeset
  and `object_paths` rows only. Readability stays per repo (§9.3): a fork MUST NOT make
  content that exists only in the fork readable through its parent, or the reverse. Opening a
  change request from a fork publishes that CR's stack to the target (its rows are copied
  there), as GitHub's `refs/pull/<n>/head`; redaction applies to every member of the network.

### 11.2 Change requests

A change request (CR) is the review object for one ChangeId stack.

- `{id, repo_id, target_ref, change_ids[], state: open | landed | closed, author, title, body}`.
- **Revisions:** each time the stack's head changes (amend, rebase, new commit), the CR gains a
  revision `{n, head changeset id, base changeset id, created_at}`. Reviews, checks and
  classifications attach to revisions, so "what changed since my review" is a diff between
  revisions.
- A CR **lands** through Land (§7.4) on behalf of its author or a landing principal; policy
  decides whether the current revision's approvals and checks are sufficient.
- Stacks: a CR covers one linear stack; dependent CRs reference their parent CR and land in order.

### 11.3 Semantic diff classes

`diff_class` (§8) labels every path in a revision with one class, and the revision with the
highest-risk class present:

| Class | Meaning |
|---|---|
| `generated` | matches a generated-path rule in policy (lockfiles, snapshots, codegen) |
| `docs` | documentation paths |
| `comment_only` | only comments and whitespace changed (language-aware where supported) |
| `test_only` | only test paths changed |
| `rename_only` | content-identical moves |
| `dependency` | dependency manifests and lockfiles |
| `logic` | anything else |
| `sensitive` | touches a path policy marks sensitive (auth, payments, policy itself, CI config) |

Classification is deterministic and versioned. Land rules (§9.4) use classes to decide which
approvals are required; for example "agents may land `docs | test_only | comment_only` without
a human; `logic` needs one owner approval; `sensitive` needs two humans".

**Risk order** (low to high): `generated < docs < comment_only < test_only < rename_only <
dependency < logic < sensitive`. A revision's class is the maximum over its paths; an empty
change has none. Wire strings are the names in the table. The algorithm below is version
`classify-v1` (`CLASSIFIER_VERSION`); any change to it that can alter a class bumps the version,
and stored classifications carry the version that produced them.

**Inputs.** One change per path: the path (the new path, or the deleted one), the old and new
`{bytes, mode}` (either may be absent), and `renamed_from` when the caller's rename detection
paired a delete with an add. The caller also passes path rules: `generated` and `sensitive` from
policy (§9.4), and `docs`, `tests` and `dependencies` lists that default to the built-in ones
below. Globs use the policy glob syntax (§9.4): root-anchored, `*` `?` `**`, and a glob also
covers everything under a matching directory. The *involved paths* of a change are its path and
its `renamed_from`.

**Per-path algorithm.** The first step that applies decides.

1. `sensitive` if any involved path matches a sensitive glob. Nothing outranks it.
2. `generated` if every involved path matches a generated glob.
3. Otherwise take the lowest class among these candidates; `logic` if there are none.
   - *Path candidates.* If every involved path matches `dependencies`: `dependency` (not for
     binary content). Otherwise: `docs` if every involved path matches `docs` (binary content
     allowed, so images under `docs/` are docs), and `test_only` if every involved path matches
     `tests` (not for binary content). Dependency globs take precedence over docs, so
     `requirements.txt` is never docs.
   - *Unchanged.* Same path, same bytes, same mode: `comment_only` (nothing semantic changed).
   - *Pure move.* `renamed_from` differs from the path and bytes and mode are identical:
     `rename_only`, for any content including binary.
   - *Comments and whitespace.* Both sides are text (valid UTF-8, no NUL), plain files
     (`File` or `Executable`) with equal modes, in the same known language (below), and their
     canonical forms are equal; an absent side is empty. This yields `comment_only`, or
     `rename_only` when the path moved (a move never drops below its own class).
   Binary content (a NUL byte in the first 8000 bytes, or invalid UTF-8) therefore yields
   `logic` unless a rule above says otherwise. Mode changes, symlink retargets and gitlink
   changes are `logic`.

**Built-in lists** (replaceable per call). Docs: `*.md *.markdown *.mdx *.rst *.txt *.adoc`,
any `doc` or `docs` directory, and `LICENSE* LICENCE* COPYING* NOTICE* AUTHORS* CHANGELOG*
CONTRIBUTING* README*`. Tests: `tests test __tests__ spec testdata fixtures` directories,
`*_test.go *_test.py test_*.py *.test.{ts,tsx,js,jsx} *.spec.{ts,tsx,js,jsx} *_spec.rb
*_test.rb *Test.java *Tests.java *Test.kt *Tests.cs *_test.rs *_test.cc *_test.cpp`.
Dependencies: `Cargo.toml Cargo.lock package.json package-lock.json npm-shrinkwrap.json
yarn.lock pnpm-lock.yaml bun.lockb go.mod go.sum requirements*.txt constraints*.txt
pyproject.toml poetry.lock uv.lock Pipfile(.lock) setup.py setup.cfg Gemfile(.lock) *.gemspec
pom.xml build.gradle(.kts) settings.gradle(.kts) gradle.lockfile *.csproj *.fsproj *.vbproj
packages.lock.json Directory.Packages.props composer.json/lock mix.exs/lock Package.swift
Package.resolved Podfile(.lock) pubspec.yaml/lock`, each at any depth.

**Languages** (by extension, case-insensitive): Rust `rs`; C and C++ `c h cc cpp cxx hpp hh
hxx`; Go `go`; Java `java`; JavaScript and TypeScript `js jsx mjs cjs ts tsx mts cts`; Python
`py pyi`; Ruby `rb rake gemspec Gemfile Rakefile`; Shell `sh bash zsh`; YAML `yaml yml`; TOML
`toml`; CSS `css`; SQL `sql`. Any other file, including Markdown outside the docs rules, has no
comment syntax and is `logic` unless a path candidate applies.

**Canonical form.** Scan the text once, left to right, after dropping a leading BOM:

- Comments are removed: `//` and `/* */` (nested for Rust only) for the C-like languages, `/* */`
  alone for CSS, `--` and `/* */` for SQL, `#` for Python, Ruby, Shell, YAML and TOML (not a
  shebang on line 1; Shell needs the `#` at a word start, YAML after whitespace; Ruby not after
  `$` or `?`), and Ruby `=begin`..`=end`. A block comment acts as one separating space. Ruby
  text after `__END__` is data.
- Literals are copied verbatim, so a marker inside one is not a comment and an edit inside one
  is a change: `"` and `'` strings with backslash escapes (not Shell `'`, TOML `'`, SQL, or YAML
  `'`, which have none); Rust `r#"..."#`, `b`/`c` prefixes and char literals (a `'` that is not
  a char literal is a lifetime); C++ `R"d(...)d"` and digit separators; Go backtick strings;
  Java text blocks; Python and TOML triple quotes; JavaScript templates with nested `${...}`
  and regex literals (recognised after an operator, `(`, `,`, line start or a keyword such as
  `return`); Ruby `#{...}` interpolation, percent literals and regexes; Shell `$(...)`
  inside `"`, `$'...'`, backticks and heredocs; YAML block scalars; SQL `"..."`, backticks and
  `$tag$...$tag$`; Ruby and Shell heredoc bodies up to their terminator line. A literal that is
  never closed runs to the end of the file.
- Whitespace: blank lines and trailing whitespace are dropped; line breaks are kept (a join of
  two lines is a change); inside a line a run of whitespace becomes one space only between two
  identifier characters or two operator characters, and is dropped elsewhere; leading
  indentation is dropped except for Python and YAML, where it is kept byte for byte; `\r` is
  whitespace. Every non-empty line ends with `\n`.

The scan is a pure function of the bytes: no clock, randomness or hash-map order.

### 11.4 Reviews

`{id, cr_id, revision, reviewer, verdict: approve | request_changes | comment, comments[]}`.
Comments anchor to `(path, side, line range)` on a revision and are carried forward to later
revisions when the anchored lines are unchanged. Agents can review; policy decides whether an
agent's approval counts toward a rule.

### 11.5 Path leases

A lease announces intent to change paths before the work starts.

- `{id, repo_id, ref, paths[], holder, change_id?, mode: advisory | exclusive, expires_at, note}`.
- `AcquireLease` succeeds unless an exclusive lease held by someone else overlaps; overlapping
  advisory leases succeed and return the overlaps so the agent can coordinate or route around
  them.
- Leases expire (default 2 hours, renewable) and are released when their change lands or
  their CR closes.
- Policy may require an exclusive lease to land changes under certain paths. A land touching an
  exclusively leased path held by another principal fails with `LEASED`.
- Leases are events (§11.7), so other agents learn about them without polling.

### 11.6 Checks and the resolver hook

- **Check runs:** `{id, repo_id, changeset, name, status: queued | running | passed | failed | skipped, url, summary, started_at, finished_at}`.
  External CI reports through the API. Required checks come from policy.
- **Post-land verification** (default: detect and revert). Checks reported on pushes of the
  default ref are `post_land`. A changeset is **green** when it has `post_land` runs and the
  latest run of every name passed; reporters announce their checks (queued) before any of them
  finishes, as CI does when it creates a run.
  - `green/<ref>` moves forward only: to a green changeset reachable from the ref's tip, never
    to an ancestor of its current target and never sideways while that target is still on the
    ref. Each move is a plain CAS of the current target; repeating a decision is a no-op.
  - When a `post_land` check fails on a changeset of the default ref the server first re-runs
    the failing check at that changeset once. If it passes, the session ends `FLAKY`
    (`bisect.flaky`, audit `bisect.flaky`) and nothing is blamed or reverted. Otherwise it
    bisects the landed range (the plan is a pure function of the probe outcomes), identifies the
    first failing changeset, maps it to the CR whose land put it on the ref (any member of a
    multi-changeset stack, through the forge's landed index), and opens a revert of that CR
    (landed at once with the repo setting `auto_revert`). One session per failing check run.
  - **Deploys from tested commits.** An environment with `require_green` refuses a deployment
    of a changeset that is not green (`POLICY_DENIED`, rule `require_green`);
    `CreateDeployment.deploy_green_tip` deploys the ref's newest green changeset instead. A
    workflow job deploying to such an environment waits while post-land checks of its changeset
    reported by anything but its own run are pending or a bisect of the ref runs, and fails when
    one of them failed or when nothing tested the changeset (its own run counts only as a push of
    the default ref, through `needs`).
  - **Opt-in prevention** (repo setting `merge_queue_check_combined`): the merge queue tests
    what would land. The head entry's stack is rebased onto the target tip, the entry is
    `CHECKING` with that result as `speculative_head`, `merge_queue.checks_requested` (workflow
    event `merge_group`) runs checks on it, and the required checks are read from it: a failure
    ejects the entry (`POLICY_DENIED`, a comment names the checks), and on success exactly that
    changeset lands. A result built on an older tip is rebuilt. Entries are tested one at a
    time in queue order.
- **Resolver hook:** a repo may register a resolver endpoint (any agent). When a land or rebase
  hits a conflict, the server sends the structured conflict (§7.3.2) and accepts a resolution
  changeset, which goes through the same checks and policy as any other change.

### 11.7 Events

Each repo has an append-only event log with a monotonically increasing sequence number.
Event kinds include `ref.updated`, `cr.opened`, `cr.revised`, `cr.landed`, `review.submitted`,
`check.updated`, `lease.acquired`, `lease.released`, `policy.changed`, `token.revoked`,
`cr.brief_set` (body: the change request) and `evidence.added`, `evidence.updated`,
`evidence.deleted` (§11.9; `payload_json` is the `bee.v1.Evidence`, without signed URLs). The
subject of a brief or evidence event is its change request, so a screen showing that change
request refreshes on them.

- `Subscribe(repo, from_seq, kinds[])` streams events and resumes from any sequence still
  retained (default 30 days). Clients never need to poll.
- Webhooks deliver the same events with HMAC signatures and at-least-once retries.

### 11.8 Issues

Beehive keeps issues deliberately thin: `{id, repo_id, number, title, body, state, labels[],
milestone?, assignees[], author}` with comments. They exist so GitHub imports keep their numbers
and links, and so a change request can point at the task it implements. Bodies are Markdown;
`body_html` (on request) is a sanitized rendering: raw HTML escaped, links and images only for
`http`, `https`, `mailto` and relative URLs. Planning (boards,
sprints, roadmaps) belongs in external trackers through adapters.

### 11.9 Briefs and evidence

A change request should show what it does, not only how: people supervising agents read the
outcome and look at the result, and read code only when something calls for it. A CR therefore
carries a **brief** in words and **evidence** in media, and the server computes an advisory
**checklist** of what is still owed. None of it is consulted by landing (§7.4): it nudges, it
never gates. `BriefService` (`proto/bee/v1/briefs.proto`) serves it.

- **Brief** (`ChangeRequest.brief`): `{outcome, highlights[{text, paths}], asks[{question,
  options}], risks[{text, mitigation}], rollback, try_it_url}`, plus `written_for_revision`,
  `author` and `updated_at`, which the server sets. It is written by `SetChangeBrief` or with the
  CR (`OpenChangeRequest.brief`), replacing the previous brief, and emits `cr.brief_set`. Limits:
  outcome and every text at most 280 characters, at most 20 highlights, asks and risks, 2 to 6
  options per ask, `try_it_url` https. An answer to an ask is an ordinary comment
  `Decision on "<question>": <option>`, so agents read decisions where they read every comment.
- **Evidence** is a list of items per CR: `{id, revision, head, spec, assets[], source,
  check_run_id, workflow_artifact_id, uploader, state, stale, created_at, updated_at}`. The spec
  names a **kind** (screenshot, before/after, video, benchmark, image set, audio, document,
  preview link, test report) with a title and caption, benchmark series (`{name, unit, baseline,
  value, lower_is_better}`), a test report, a URL, paths, a hero flag and an order. Each kind
  allows a fixed set of assets by role (main, before, after, poster, captions); anything else is
  `INVALID_ARGUMENT` (subcode `bad_evidence`), and every image needs alt text (`missing_alt`).
- **Revisions and staleness.** An item records the revision it was captured for (the current one
  unless the request names another). It is **stale** when the CR has a newer revision; staleness
  is computed on read, never stored, and stale items stay visible, labelled.
- **Sources.** `UPLOADED` evidence comes from the caller and is shown as "From the agent" (or the
  person who uploaded it). `CHECK_RUN` evidence names a check run: the caller must be its
  reporter and the run's changeset the head of one of the CR's revisions, which becomes the item's
  revision. `WORKFLOW_ARTIFACT` evidence copies files out of an artifact of a workflow run on the
  CR. Both CI sources are shown as "Verified by CI", and CI evidence can be deleted only by its
  reporter or a repo maintainer.
- **Writes.** Setting the brief and adding, updating or deleting evidence need the right to edit
  the CR (its author, or repo Write), and each write is audited. Bytes arrive inline (at most
  8 MiB per request, `AddEvidence`), from a workflow artifact, or by `BeginEvidenceUpload`
  (the item is `PENDING`, one signed PUT URL per asset, valid 15 minutes) then
  `FinalizeEvidence`, where the server computes each digest and answers `INTEGRITY` when it
  differs from the caller's; an item left `PENDING` for an hour is deleted. Adding or finalizing
  emits `evidence.added`; updating (spec and alt text only; the kind and files never change,
  `kind_immutable`) emits `evidence.updated`; deleting emits `evidence.deleted`.
- **Safety.** Uploads are allow-listed by content type and checked by sniffing the bytes: png,
  jpeg, webp, gif; mp4, webm; mpeg, ogg, wav; pdf; plain text, Markdown, CSV and WebVTT. SVG,
  HTML and script are never accepted (`bad_media`). Downloads are signed URLs the server serves
  itself, carrying the principal chain they were issued for and decided again when redeemed
  (§9.6); responses carry `X-Content-Type-Options: nosniff`, `Content-Security-Policy:
  default-src 'none'; sandbox`, `Content-Disposition: attachment` for anything but images, video
  and audio, and support `Range`.
- **Limits** (`TOO_LARGE` with `limit_exceeded`): image 10 MiB, video 100 MiB, audio 25 MiB,
  document 20 MiB; per CR at most 64 items and 512 MiB of assets.
- **Checklist** (`GetBriefChecklist`, and returned by `SetChangeBrief`): `{revision, complete,
  items[{id, detail, kinds, paths, satisfied, evidence_ids}]}`, a pure function of the revision's
  per-path diff classes (§11.3), the brief and the evidence captured for that revision:
  - `brief`: the CR is agent-authored and a path is `logic` or `sensitive` → an outcome and at
    least one highlight;
  - `brief:risk`: a path is `sensitive` → at least one risk or a rollback;
  - `evidence:ui`: a UI file (`*.tsx`, `*.jsx`, `*.vue`, `*.svelte`, `*.astro`, `*.html`, `*.css`,
    `*.scss`, `*.less`) is `logic` or `sensitive` → a screenshot, before/after or video.
  `detail` says what is owed in words ("a screenshot of the UI change"); `paths` lists at most 20
  paths that raised the item, in path order. A checklist with no items is complete.

### 11.10 Swarms and repository work

A swarm records one repository, a name, the person's goal, an optional issue number in that
repository, and a shared branch. Its lead is an active agent run on that branch. The lead's
human owner answers for the swarm. The public interface names the lead “Queen · lead”.
An agent run belongs to at most one swarm. Independent runs remain available without a swarm.

The lead plans tasks with titles and claimed paths. Each attempt records the member run,
its agent, when it started and finished, and the outcome and reason. The hand-back records
done, deviations, concerns, and follow-ups. An attempt is ended by its 1-based number. A task
can be open, working, finished, or empty. A refused or empty attempt leaves the task eligible
for another attempt. When a member run becomes terminal, Beehive closes its active attempt
and releases its path claim in the same transaction. A recorded hand-back makes the attempt
finished. Otherwise it comes back empty with the run's one-line reason if available.

Any authenticated principal with repository read may offer an agent they control and a run
budget. The lead, its owner, or a repository admin decides. The requester and the joining
agent cannot decide the request. A non-human request requires acceptance by the owner or
another person with repository admin access. Acceptance checks the `swarm_join` policy action and
queues a member run on the shared branch using the existing run budget and access rules.
A decision refused by policy is recorded as refused. Beehive records the aggregate budget
and cost from member runs. These values are visible to the lead, owner and repository admins.

The owner or a repository admin can start, pause, resume, stop, or finish the swarm.
Pause holds queued members and new attempts. In-flight attempts can hand back during a pause.
Resuming a tripwire pause requires the answerable person to acknowledge its Safety attention item.
Stop cancels member runs through the agent run cancellation path. Finish requires every
member run and task to have ended. The lead alone writes the status line. A person's steering
message goes to the lead's run session.

`ListRepoWork` returns a tagged union of swarms and independent runs ordered by newest activity.
Each entry carries the ids of the pull requests it produced. Swarm mutations appear in
`EventService` as `swarm.*` events on the swarm subject. Run activity remains available through
the existing agent run events. The wire contract is `proto/bee/v1/swarms.proto`.

### 11.11 Safety record and human intervention

Beehive records what was asked and the evidence of what was done for each swarm or
independent run. It derives one of five verdicts: `as_asked`, `more_than_asked`,
`less_than_asked`, `something_else`, or `needs_a_look`. A person can record a verdict
override with a note. Findings and independent review proposals are auditable.

Severe findings of selected kinds interrupt work and reach the person who answers for it.
An agent can stop its own run and request a human decision by raising a concern. An
organization owner can freeze repo ref publication by agents and services and interrupt
agent work across the organization. The freeze covers direct ref mutations, Git
receive-pack, and landing. It also holds automatic publication of a revision attributed to
an agent or service, even if a person enabled auto-merge. Direct human publication remains
subject to normal policy. With Beehive enabled, non-human ref publication requires a
working safety fence and fails closed when Beehive is unavailable. Bee with Beehive
disabled retains its standalone behavior.
The evidence, authorization and intervention contract is in
[the Beehive safety record](docs/spec/forge-safety.md).

---

## 12. Protocol

### 12.1 Transport

- One protobuf schema, `proto/bee/v1/`, served two ways from the same service objects:
  - **gRPC** (and gRPC-Web) by tonic, for typed clients and between tiers.
  - **HTTP/JSON in Connect-protocol shape**, for curl, browsers, `@connectrpc/connect-web`
    clients and simple agents. The gateway lives in `bee-proto` (`connect` module) and is an
    axum `Router` that `bee-server` mounts; it transcodes each request into a gRPC call on
    the same tonic service, so the two surfaces cannot diverge.
- **Connect unary:** `POST /bee.v1.<Service>/<Method>` with `Content-Type: application/json`
  and a proto3-JSON body (`application/proto` with a binary body is also accepted). Success is
  HTTP 200 with the response message in the same codec. An empty body means `{}`. The optional
  `Connect-Protocol-Version` header must be `1` when present; `Connect-Timeout-Ms` becomes the
  gRPC deadline.
- **Connect streaming** (server-streaming, and half-duplex client streaming for
  `UploadObjects`): `Content-Type: application/connect+json` (or `application/connect+proto`).
  Every message is an envelope of one flags byte, a 4-byte big-endian length and the payload.
  The response is HTTP 200 and ends with an envelope flagged `0x02` holding the JSON object
  `{"error"?: <Connect error>, "metadata"?: {name: [values]}}`. New RPCs are unary or
  server-streaming only, because browsers cannot send request streams.
- **Metadata:** request headers other than transport headers are forwarded as gRPC metadata.
  Unary responses return gRPC trailers as `trailer-<name>` headers; streaming responses put
  them in the end-stream `metadata`.
- Only identity content encoding is served on the JSON surface; gRPC may negotiate gzip.
- proto3-JSON rules apply: lowerCamelCase field names (original names also accepted), 64-bit
  integers as decimal strings, `bytes` as base64, enums by value name, `Timestamp` as RFC 3339.
  Unknown fields are ignored so older servers accept newer clients.
- `schemas/openapi/bee.v1.json` (OpenAPI 3.1) describes the whole JSON surface and
  `schemas/v1/messages/<message>.schema.json` holds one self-contained JSON Schema per request
  and response message. Both are generated from the descriptors by `bee-proto-gen`, and a
  test fails when they are stale.
- TLS required except on loopback and unix sockets. Auth via `Authorization: Bearer <biscuit>`.
- Large reads (`GetObjects`, `ReadPath`, `Diff`) are server-streaming.
- Response trailers carry `bee-policy-epoch` and `bee-redaction-epoch`, and `bee-tier`
  naming the tier that answered (`server`, `regional` or `daemon`). `GetServerInfo` is
  anonymous; naming a repo in it needs read access and returns that repo's two epochs.
- **Compatibility:** changes to `proto/bee/v1` are additive only: no field is renumbered,
  reused, retyped or removed, and no RPC changes its types. A test compares every build with
  the frozen descriptor of each published version.
- **Breaking changes** ship as a new package (`bee.v2`) whose services are registered on the
  same listener next to `bee.v1`: routing is by the full method path
  `/<package>.<Service>/<Method>`, so both versions are served side by side until v1 is retired
  by a recorded decision; a package or service the server does not serve answers
  `UNIMPLEMENTED`. Readers ignore unknown fields (old servers accept new clients, old clients
  read new responses); a request missing a field the RPC requires fails with
  `INVALID_ARGUMENT` and subcode `missing_field`, never with a default silently applied.

### 12.2 Errors

Every failed RPC carries a `bee.v1.ErrorDetail`: `reason` (one of the names below), a
message, `retryable`, an optional `subcode` (snake_case, for example `missing_field`,
`bad_json`, `noop_put`), `retry_after_ms`, `request_id`, and a typed payload where the table
names one. Reasons are part of the contract; codes alone are not enough for an agent to decide
what to do. `retryable` means the identical request may succeed if sent again (SPEC §15.5);
clients never retry other reasons without changing the request.

On gRPC the detail is a `google.rpc.Status` detail in `grpc-status-details-bin`. On the Connect
surface the error body is `{"code", "message", "details": [{"type": "bee.v1.ErrorDetail",
"value": <base64 protobuf, unpadded>, "debug": <proto3-JSON>}]}` with the HTTP status of the
code. If a status reaches the gateway without a detail (for example tonic's own `UNIMPLEMENTED`
for an unknown method), the gateway adds one with the default reason for the code: `CANCELLED`,
`INVALID_ARGUMENT` (also for `OUT_OF_RANGE`), `DEADLINE_EXCEEDED`, `NOT_FOUND`,
`ALREADY_EXISTS`, `PERMISSION_DENIED`, `RESOURCE_EXHAUSTED`, `INVALID_STATE` (for
`FAILED_PRECONDITION`), `VERSION_MISMATCH` (for `ABORTED`), `UNIMPLEMENTED`, `UNAVAILABLE`,
`INTEGRITY` (for `DATA_LOSS`), `UNAUTHENTICATED`, and `INTERNAL` otherwise.

The reasons are the `bee.v1.ErrorReason` enum (wire name = value name without
`ERROR_REASON_`). This table, the enum comments in `common.proto` and `bee_proto::errors`
are checked against each other by tests. Reasons may be added, never renamed or removed.

| Reason | gRPC code | Connect code | HTTP | Retryable | Meaning and payload |
|---|---|---|---|---|---|
| `UNAUTHENTICATED` | UNAUTHENTICATED | unauthenticated | 401 | no | No credential, or it does not verify |
| `TOKEN_EXPIRED` | UNAUTHENTICATED | unauthenticated | 401 | no | Token past its expiry |
| `TOKEN_REVOKED` | UNAUTHENTICATED | unauthenticated | 401 | no | Token or an ancestor revoked |
| `PERMISSION_DENIED` | PERMISSION_DENIED | permission_denied | 403 | no | Principal lacks the permission |
| `POLICY_DENIED` | PERMISSION_DENIED | permission_denied | 403 | no | A policy rule refused; `policy_denial` lists what is missing |
| `BUDGET_EXHAUSTED` | RESOURCE_EXHAUSTED | resource_exhausted | 429 | no | Token budget spent (§9.2) |
| `RESOURCE_EXHAUSTED` | RESOURCE_EXHAUSTED | resource_exhausted | 429 | yes | Throttled (§15.4); `retry_after_ms` and `retry-after` |
| `QUOTA_EXCEEDED` | RESOURCE_EXHAUSTED | resource_exhausted | 429 | no | Quota or spending limit reached; `limit_exceeded` |
| `NOT_FOUND` | NOT_FOUND | not_found | 404 | no | Absent or not visible to the caller |
| `MISSING_OBJECT` | FAILED_PRECONDITION | failed_precondition | 400 | no | Referenced objects have no row; `missing_objects` |
| `REDACTED` | NOT_FOUND | not_found | 404 | no | Object redacted (§13) |
| `ALREADY_EXISTS` | ALREADY_EXISTS | already_exists | 409 | no | Name or key taken |
| `ALREADY_LANDED` | FAILED_PRECONDITION | failed_precondition | 400 | no | ChangeId landed from another source (§7.4) |
| `EMPTY_CHANGE` | FAILED_PRECONDITION | failed_precondition | 400 | no | Result would be empty without `allow_empty` |
| `MERGE_CONFLICT` | FAILED_PRECONDITION | failed_precondition | 400 | no | Conflicts as data; `conflicts` (§7.3.2) |
| `AMBIGUOUS_MERGE_BASE` | FAILED_PRECONDITION | failed_precondition | 400 | no | No usable merge base |
| `LEASED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Exclusive lease held by another; `lease_conflict` (§11.5) |
| `VERSION_MISMATCH` | ABORTED | aborted | 409 | no | Stale expected version; `version_mismatch` has the current one |
| `NOT_FAST_FORWARD` | FAILED_PRECONDITION | failed_precondition | 400 | no | Non-fast-forward without `allow_non_fast_forward` or `force` |
| `AMBIGUOUS_PREFIX` | INVALID_ARGUMENT | invalid_argument | 400 | no | Prefix matches several ids; `ambiguous_prefix` |
| `INVALID_ARGUMENT` | INVALID_ARGUMENT | invalid_argument | 400 | no | Malformed request; `subcode` names the rule |
| `TOO_LARGE` | INVALID_ARGUMENT | invalid_argument | 400 | no | Size or count limit (§4.6); `limit_exceeded` |
| `INTEGRITY` | DATA_LOSS | data_loss | 500 | yes | Bytes did not match their id |
| `ROOT_TREE_MISMATCH` | FAILED_PRECONDITION | failed_precondition | 400 | no | Computed root differs from `expected_root_tree` |
| `MULTI_PARENT` | FAILED_PRECONDITION | failed_precondition | 400 | no | Rebase of a multi-parent changeset |
| `MERGE_IN_STACK` | FAILED_PRECONDITION | failed_precondition | 400 | no | Merge changeset in a stack the land method cannot take |
| `LAND_CONTENTION` | UNAVAILABLE | unavailable | 503 | yes | Land attempt budget ran out |
| `UNAVAILABLE` | UNAVAILABLE | unavailable | 503 | yes | Dependency or primary unreachable |
| `DEADLINE_EXCEEDED` | DEADLINE_EXCEEDED | deadline_exceeded | 504 | yes | Deadline passed |
| `CANCELLED` | CANCELLED | canceled | 499 | yes | Caller cancelled |
| `UNIMPLEMENTED` | UNIMPLEMENTED | unimplemented | 501 | no | Service or method not served here (unknown paths answer 404) |
| `INTERNAL` | INTERNAL | internal | 500 | no | Server bug; `request_id` locates it |
| `NOT_QUARANTINED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Unredact or purge outside quarantine (§13) |
| `NOT_REDACTABLE` | FAILED_PRECONDITION | failed_precondition | 400 | no | Object kind cannot be redacted |
| `SECRET_DETECTED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Push protection; `push_protection` lists findings |
| `ARCHIVED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Repo is archived |
| `LOCKED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Conversation locked |
| `BLOCKED` | PERMISSION_DENIED | permission_denied | 403 | no | Caller blocked by the owner |
| `INTERACTION_LIMITED` | PERMISSION_DENIED | permission_denied | 403 | no | Interaction limits exclude the caller |
| `DRAFT` | FAILED_PRECONDITION | failed_precondition | 400 | no | Change request is a draft |
| `GIT_DISABLED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Git access off for the repo |
| `SSO_REQUIRED` | PERMISSION_DENIED | permission_denied | 403 | no | Org requires an SSO session |
| `TWO_FACTOR_REQUIRED` | PERMISSION_DENIED | permission_denied | 403 | no | Org requires two-factor auth |
| `IP_NOT_ALLOWED` | PERMISSION_DENIED | permission_denied | 403 | no | Source address not on the allow list |
| `APPROVAL_REQUIRED` | FAILED_PRECONDITION | failed_precondition | 400 | no | Human approval needed first |
| `INVALID_STATE` | FAILED_PRECONDITION | failed_precondition | 400 | no | Object in the wrong state for the operation |

The reason table, with each reason's gRPC code and default `retryable` flag, is
`bee_core::error::Reason`. `INVALID_ARGUMENT` errors name a subcode (`bee_core::error::Subcode`,
e.g. `INVALID_PATH`, `NON_CANONICAL`, `DELTA_MISMATCH`) in the payload under the key `subcode`,
so the error document keeps its four fields.

### 12.3 Services

Every PRODUCT §4 area has API surface; the protos are the reference for each RPC.

| Service | File | Purpose |
|---|---|---|
| `ObjectService` | vcs | GetObjects (range, encoding, per-item status, access class), HasObjects, ResolveRef (`at_version`, `min_version`), server info, tier stats, daemon snapshots. Served by every tier. |
| `QueryService` | vcs | GetChangesets, ReadPath, ListTree, Log, Blame, Diff, IsAncestor, MergeBase, Resolve, GetDerived, derived types, compare, copy detection |
| `WriteService` | vcs | UploadObjects, CreateChangeset (`expected_root_tree`, import mode), Rebase, Merge, Land (merge methods), Revert |
| `RefService` | vcs | ListRefs, CreateRef, UpdateRef (`allow_non_fast_forward`), DeleteRef, RenameRef, GetRefLog; reasons recorded for audit |
| `OrgService` | forge | Orgs, members, roles, invitations, outside collaborators, settings, rename, delete and restore |
| `RepoService` | forge | Repos, settings, archive, transfer, templates, forks, topics, autolinks, custom properties, collaborators, resolver hook, mirrors, deploy keys, delete and restore, repo context |
| `ChangeService` | forge | Change requests: drafts, reopen, edit, review requests, reviews, threads, suggestions, viewed files, labels, linked issues, templates, land, auto-merge, merge queue, revert, stacks, commit comments, land conflicts for resolvers |
| `CheckService` | forge | Check runs with output and annotations, suites, rerequest, commit statuses, post-land bisect |
| `LeaseService` | forge | Acquire, renew, release, get and list leases |
| `EventService` | forge | Subscribe, event log paging, repo/org webhooks with deliveries, ping and redelivery |
| `CommentService` | content | Edit, delete and edit history of any comment; reactions; read markers |
| `AuthService` | auth | Login, second factors, WhoAmI, CheckAccess, CheckObjectAccess (caches), agents, tokens, signing keys |
| `AdminService` | auth | Redact (lifecycle, purge schedule), unredact, PurgeNow, GC, fsck, backups and restores, signing-key rotation |
| `PolicyService` | policy | Effective policy, validate, evaluate, rulesets with bypass and evaluate mode, rule insights, custom roles, CODEOWNERS |
| `ImportService` | import | Imports from GitHub and other forges, phases, fidelity report, identity claims, exports |
| `GitService` | git | Git gateway: GetGitObjects, GitListRefs, GitPlanFetch, GitResolve, GitIngestCommits, LFS batch, preflight, credentials |
| `IssueService` | issues | Issues, comments, labels, milestones, types, sub-issues, dependencies, fields, templates, pin, lock, transfer, timeline, saved replies |
| `ProjectService` | projects | Projects: items, fields, views, iterations, workflows, insights, status updates |
| `DiscussionService` | discussions | Categories, discussions, answers, polls, pins, conversions |
| `WikiService` | wiki | Wiki pages and history on the sibling wiki repo |
| `GistService` | gists | Gists, revisions, forks, stars, comments |
| `ReleaseService` | releases | Releases, assets, generated notes, immutable releases, tags, source archives |
| `NotificationService` | notifications | Inbox, subscriptions, watching, settings, live inbox stream, reminders |
| `SearchService` | search | Code, symbol, issue/CR (qualifier language), repo, commit, user, topic, discussion and package search; code navigation; saved searches; trending |
| `InsightsService` | insights | Languages, license, community profile, citation, pulse, contributors, traffic, network |
| `UserService` | users | Profiles, follows of users and repos, stars and lists, emails, SSH and signing keys, settings, sessions, security log, account lifecycle |
| `BriefService` | briefs | Briefs of change requests, evidence (screenshots, before/after, video, benchmarks, test reports, preview links) with uploads, the advisory checklist (§11.9) |
| `PostService` | posts | Posts of their own or on change requests (summary, Markdown body, article, images, video, attachments: change requests, a release, an issue, an agent run), approval of agent posts, likes, reposts, the Following feed, adoptions |
| `SponsorService` | sponsors | Sponsor profiles, tiers, sponsorships, FUNDING.yml |
| `WorkflowService` | workflows | Workflows, dispatch, runs, jobs, logs, artifacts, caches, Actions policies, required workflows, OIDC, attestations |
| `RunnerService` | workflows | Self-hosted runners, groups, machine classes, native runner job protocol |
| `SecretService` | secrets | Secrets and variables at org, repo, environment, workspace and agent scope |
| `DeploymentService` | deployments | Environments with protection rules, deployments, statuses, reviews, green gating |
| `PackageService` | packages | One package model across registries: packages, versions, access, provenance |
| `PagesService` | pages | Sites, builds and deployments, custom domains, certificates |
| `SecurityService` | security | Dependency graph, SBOM, advisories and alerts, updates, dependency review, code scanning (SARIF), secret scanning, push-protection bypass, repo advisories, overview, campaigns, signature verification |
| `AgentService` | agents | Agent registry, dashboards, runs and sessions, instructions, memory, context bundles, component watchers and anomalies, feed triggers |
| `SwarmService` | swarms | Swarm coordination, task attempts, hand-backs, admission, lead steering, and repository work |
| `SafetyService` | safety | Work evidence, findings, independent review, concerns, attention, merge freeze, sampling, and summaries |
| `WorkspaceService` | workspaces | Cloud workspaces, prebuilds, ports, dotfiles, egress policy |
| `ModelService` | models | Model gateway: catalog, inference, prompt files, evals |
| `TeamService` | teams | Teams, nesting, members, repo access, review assignment, IdP sync |
| `AppService` | apps | Apps, installations and tokens, OAuth apps and grants, marketplace |
| `EnterpriseService` | enterprise | Enterprise accounts, SAML, SCIM, IP allow lists, verified domains, 2FA, policies |
| `BillingService` | billing | Plans, seats, usage, budgets, invoices, cost centers |
| `ModerationService` | moderation | Blocks, reports, interaction limits, minimize, lock, moderators, flood limits, takedowns |
| `AuditService` | audit | Hash-chained audit log: query, stream, verify, SIEM sinks, export |
| `OpsService` | ops | Health, cluster status, metrics, logs, traces, config validation, rate limits, rollouts, status page |

### 12.4 CLI and MCP

- Every `bee` command accepts `--json` and then writes exactly one JSON document to stdout
  matching `schemas/v1/cli/<noun>.<verb>.schema.json`, or an error document matching
  `schemas/v1/error.schema.json`, with a non-zero exit code.
- `bee mcp` (stdio) and `/mcp` on the server (streamable HTTP) expose the same operations as
  MCP tools whose input and output schemas are generated from the protobuf definitions: the
  files in `schemas/v1/messages/`, or `bee_proto::schema::Catalog::message_schema` at runtime.
  MCP input schemas additionally accept a string wherever a `RepoRef` appears, including nested
  and repeated fields: `org/name` means `fullName` and a ULID means `id`; object forms remain
  accepted. This convenience spelling is normalized to the protobuf request before dispatch.

---

## 13. Redaction

Admins can **redact** a blob or meta: its bytes are deleted from storage and its id is added to
a permanent blocklist (the same content cannot be uploaded again). Ids never change. Readers
receive `REDACTED` for the object; working copies write a fixed placeholder that status treats
as clean. Each redaction bumps the repo's redaction epoch, which caches watch.

- **Lifecycle.** `Redact` quarantines (bytes kept under the quarantine prefix, unreadable);
  `Unredact` restores a quarantined object; `PurgeNow`, `Redact` with `purge_now`, or the
  scheduled purge after the quarantine window make it permanent.
- **Permissions.** Quarantining and unredacting need `redact`; purging needs `purge` as well.
  Both are `admin` actions (§9.4), checked on the token and on the caller's role, so a token
  attenuated to `redact` cannot purge. Ref management (`manage_refs`) and non-fast-forward
  moves (`force_update`) are separate actions of their own.
- **Reading the list.** `ListRedactions` needs nothing beyond seeing the repo: any valid token
  for a public repo, read access to a private one (a stranger is told the repo does not exist,
  §9.6). It needs neither `redact` nor read access to the redacted paths: ids are not content.
  `Rebase` (also `commit`), `IsAncestor`, `MergeBase`, `GetDerived` and `Resolve` need read.
- **Carried forward.** Trees name entries by id, so a changeset may carry a redacted or purged
  entry forward unchanged: CreateChangeset, Rebase and Land over a tree holding one succeed,
  the entry keeps its id, and fsck treats it as valid. A delta that *puts* a redacted id is
  refused (`MISSING_OBJECT`): no new reference to redacted content can be made.
- **Consumers.** A consumer keeping a redaction set lists with `since_epoch` = one above the
  highest epoch it has seen. Each record carries its current state and the epoch of its last
  change; an `UNREDACTED` record removes the id. A consumer that sees the repo's epoch go
  below the one it holds (a restore), or that cannot tell what it missed, discards its set and
  re-lists from epoch 0. Caches holding per-object decisions drop them all at any epoch change.
- **Derived data.** Every derived type declares whether it is **content-bearing** (its values
  depend on blob contents: `blame`, `diff_class`, `copy_detection`, `language_stats`) or not
  (`graph`, `path_history`, `git_mapping`). Quarantine keeps derived records. A purge deletes
  every record of every content-bearing type in each repo that referenced the object; lazy
  types derive again from what remains readable. Content-free records stay.
- **Working copies.** A redacted file is written as exactly the bytes
  `[bee] This file was redacted. Its content is no longer available.\n` and listed as a hidden
  path with reason `redacted`; status treats a redacted path holding exactly those bytes as
  unchanged. Every mutating command first asks for redactions made since the redaction epoch
  the working copy last applied (kept in its state): a clean file whose blob is now redacted is
  replaced by the placeholder (`redacted` in the command's report), a file with local changes
  is left alone and reported (`redactedDirty`), and an unredacted placeholder gets its content
  back. Status itself never touches the network.
- **Tooling.** `bee redact` names content every way the RPC accepts: `--path P [--at R]
  [--all-versions]` (the blob at a path, or every version of it in R's history), `--object ID`
  (blob or meta ids) and `--meta R` (the metadata of the changesets R resolves to), with a
  required `--reason`, and `--purge-after D` or `--now`. `bee admin unredact`,
  `purge-now` and `list-redactions` complete the set; every one is also an MCP tool.

---

## 14. Testing contract

Correctness is checked by independent oracles, not just by unit tests:

1. **Reference model** (`bee-testkit::model`): a small, obviously-correct implementation of
   apply-delta, merge, land ordering, graph queries and blame, with no dependency on production
   crates. Property tests compare production against it.
2. **Golden vectors** for every object kind and every error payload, frozen once published.
3. **Faithful in-memory store**: strictly serializable, with stale-replica reads and injected
   transaction failures.
4. **Named fault points** at every storage call and write-ordering step: error, crash, delay,
   and "committed, response lost".
5. **fsck** as the invariant oracle: digests, canonical encoding, invariant D, eager rows,
   graph correctness, `object_paths` completeness.
6. **Complexity gates** via storage access counters at 10³, 10⁵ and 10⁷ files: a one-file
   commit reads at most depth + c trees; rebase over an unrelated tip reads zero blobs; land
   uses a constant number of transactions; sparse checkout fetches the same count at every size.
7. **Linearizability** checks for ResolveRef / UpdateRef / Land under server crashes;
   N-way concurrent CAS has exactly one winner and no ref-log gaps.
8. **Differential tests against git**: tree listing, ignore rules, blame, `merge-file`, and
   import fidelity on real histories.
9. **Tier conformance**: the same read suite against the server and against a daemon in front
   of it, byte-identical.
10. **Deterministic simulation** of many agents (commit, lease, land, review, revert) against
    one server with a seeded clock and network faults.

Tests are hermetic by default; Postgres/S3 tests run when their environment variables are set.

---

## 15. Reliability

GitHub's availability reports from July 2025 to September 2026 record roughly 70 incidents
(summary and sources in `docs/research/github-reliability.md`). Five causes recur:
- config and automation changes;
- overloaded or mis-migrated shared database primaries;
- traffic spikes, including agent and scraper traffic;
- retry storms;
- shared edge infrastructure (DNS, load balancers, service mesh).

One fault routinely takes down git, pull requests, webhooks and CI together. Every rule below
exists to stop one of those patterns.

### 15.1 Failure domains

- **Git data path is independent of the forge.** Object reads, ref reads, checkout, fetch and
  land depend only on the blob store, the ref/changeset tables, token verification and policy.
  They MUST NOT depend on search, notifications, review data, webhooks, settings caches or any
  forge table. A Beehive outage leaves `bee checkout`, `bee land` and `git clone` working.
- **Reads survive write outages.** Immutable objects and changesets are served from any replica
  or cache tier. With the metadata primary down, reads of refs return the last known version
  marked `stale = true` instead of failing, as long as it is no older than the configured
  staleness bound (`metadata.stale_read_max_ms`) and no older than the client's `min_version`;
  past either, the read fails with `UNAVAILABLE`.
- **Decoded-object caches.** Each server process remembers the decoded rows it reads:
  changesets, parent edges and derived rows (immutable, kept at any age) and object rows,
  `object_paths`, refs, org and repo records, redaction records and epochs (mutable, served
  only within `metadata.stale_read_max_ms` of being read). While the metadata store answers
  `UNAVAILABLE`, reads are answered from these rows, so history, trees and files that were
  being served keep being served (refs marked `stale`); anything never read, and every write,
  fails with `UNAVAILABLE`. Redactions, state changes and purges made through the process drop
  the rows they affect at once.
- **Forge surfaces fail independently.** Change requests, reviews, checks, leases, events and
  webhooks each own their tables and their worker pools. A backlog in one (e.g. webhook delivery)
  cannot consume another's connections or threads.
- **Search is never a dependency of a list view.** Lists of change requests, checks and leases are
  served from their own indexed tables; full-text search is optional enrichment.

### 15.2 Cells

- A **cell** is a complete, independent deployment (server processes, metadata database, blob
  bucket, workers) serving a set of orgs. Production runs many cells; dev mode is one cell.
- Every org lives in exactly one cell. A cross-cell request is a redirect, never a fan-out.
- Cells share nothing at runtime except a small, read-mostly **directory** mapping orgs to cells.
  Clients cache the directory, so cells keep serving when the directory is down (static stability).
- Config, binaries and schema changes roll out **cell by cell** with automatic rollback on SLO
  regression. A change never reaches every cell at once.
- Within a cell a rollout is **rolling**: processes are replaced one at a time while the cell
  serves, so old and new releases run side by side over the same stores. Every write path is
  safe for that (idempotent writes, the fenced coalescer, the ref CAS), so a rolling upgrade
  under load loses and duplicates nothing; each process records a binary or config rollout
  when it starts on a new version.

### 15.3 Data rules

- **No shared global primary.** All metadata is keyed by org, then repo; one hot repo can saturate
  at most its own cell's database, and within a cell, per-repo admission control (§15.4) caps it.
- **Schema changes are expand/contract only.** A column or table is dropped only after a release
  in which no code reads it, verified by query logs. Backfills are resumable and cancelling one
  never drops data.
- **64-bit or wider keys everywhere.** No 32-bit sequence or lookup key in any table.
- **Every call has a deadline.** Database queries, RPCs and object fetches carry deadlines; no
  unbounded waits.
  A server-streaming RPC (`Subscribe`, `WatchWorkflowRun`, `TailWorkflowJobLogs`, and every
  other `returns (stream ...)`) is bounded only by a deadline its client sends (`grpc-timeout`,
  `Connect-Timeout-Ms`, capped at the maximum): without one it stays open until the client
  disconnects or the server shuts down, and the deadline of the call that opened it does not
  end it. Each wait inside the stream (a query, an access re-check) still carries its own
  deadline. A stream that ends on its client's deadline ends with `DEADLINE_EXCEEDED` whose
  message names the field to resume from (for `Subscribe`: `from_seq` set to the last event's
  `seq` plus one). Unary and client-streaming calls end at the default deadline when the client
  sends none.
- **Background jobs are admission-controlled on primary load** (CPU, connections, lock waits),
  not only on replica lag, and pause automatically.

### 15.4 Admission control and fairness

- Requests are classified into priority tiers: `interactive` (a human or an agent's foreground
  step), `land`, `bulk` (imports, backfills, mass operations), `anonymous`. Under load the server
  sheds `anonymous` first, then `bulk`; `land` and `interactive` keep their reserved capacity.
- Quotas are enforced per principal **and** per delegation root, so a thousand agents under one
  human share that human's ceiling instead of each getting a fresh one. Quotas are in the token
  budget (§9.2) and in org settings, and are visible through `WhoAmI`.
- Unauthenticated traffic runs in its own pool with its own limits and can never starve
  authenticated traffic.
- Rate limiting **fails open for authenticated traffic** when the limiter itself is unhealthy, and
  a limited decision is never cached beyond its window.
- Every throttling response carries `retry-after` and the `RESOURCE_EXHAUSTED` reason with the
  quota that was hit.
- **Algorithm and cost model.** Rate quotas are token buckets: one per principal
  (`admission.per_principal_rps`) and one per delegation root (`admission.per_root_rps`), each
  refilling continuously at its rate up to a burst of twice the rate. A request is admitted only
  if both buckets hold its cost, and then pays it from both; a refusal by either spends nothing
  from either and answers with the time until the cost is available. Every authenticated
  request costs one token; `GetObjects` and `HasObjects` cost one more per full thousand ids
  (so a 1,000-id batch costs two). Concurrency is limited separately by the priority tiers
  above (`admission.max_*`), and budgets and quotas of changesets, lands and upload bytes by
  §9.2.

### 15.5 Clients behave

- Official clients (CLI, daemon, MCP, SDKs) use a **retry budget**: retries are capped at a small
  fraction of recent successful requests, with full-jitter backoff, and honour `retry-after`.
- Retries are only issued for reasons marked `retryable` (§12.2). Writes are retried only where
  the API is idempotent (CreateChangeset, Land, UpdateRef with an expected version).
- Agents subscribe to events (§11.7) instead of polling.

### 15.6 Operations

- Control planes (cell directory, DNS, config) keep the last known-good state and refuse inputs
  that would remove more than a bounded fraction of capacity or records in one step.
- Config is immutable and versioned; running nodes are never edited in place.
- Certificates are issued automatically with short lifetimes; expiry is monitored by a system
  independent of the issuer.
- A deploy never removes capacity at peak: rollouts keep a headroom budget per cell.
- Model providers and other third parties are never on the critical path of source control.
- **Operational validation.** Before a cell serves customers its configuration passes the
  production checklist (`bee-server --check-production`, or `ValidateOpsConfig` with
  `component = "production"`): shared Postgres metadata and S3 blobs, TLS, signing and URL keys
  shared by every process, named operators, rate quotas on, no whole-filesystem imports, and
  backups off the machine. A failed error-level item blocks the rollout.
- **Tracing.** Every RPC is an OpenTelemetry server span that joins the caller's W3C
  `traceparent`, answers with `traceresponse`, and is exported over OTLP/HTTP when a collector
  is configured (`docs/spec/server.md` §6).
- **Integrity alerting.** Bytes that fail verification against their id (on any read, or in
  fsck) are never served; each failure increments `bee_integrity_failures_total{source}`,
  writes an `error` log line naming the object, and marks the `integrity` health component
  down for an hour, so an operator sees bit rot before a user does.

### 15.7 Failure classes and recovery invariants

Every failure the system is built to survive falls in one of ten classes. Each has an invariant
that holds while it lasts and after it ends, and a fault test that proves it.

| # | Class | Invariant | Fault test |
|---|---|---|---|
| 1 | A process crashes at any write-ordering step (§6.2) | No reader sees a reference to bytes that are not durable; a retry finishes or abandons the operation; fsck is clean | `crash_at_every_write_step_leaves_fsck_clean` |
| 2 | A write commits and its response is lost | Retrying the same CreateChangeset, Land or UpdateRef returns the original result; nothing is applied twice | `response_lost_on_cas_is_idempotent_for_land`; the lost responses in `store_unavailable_window_fails_retryably_then_recovers` |
| 3 | The metadata store is unavailable or aborts transactions | Writes fail with retryable `UNAVAILABLE` and change nothing; ref reads return the last known version marked stale within `metadata.stale_read_max_ms`; recovery needs no repair | `store_unavailable_window_fails_retryably_then_recovers`, `stale_ref_reads_end_at_the_staleness_bound`, `injected_transaction_failures_have_no_effect` |
| 4 | The blob store is unavailable | Object reads fail with retryable `UNAVAILABLE`; uploads leave no object row, so nothing can reference their bytes; metadata reads keep answering | `blob_store_outage_fails_retryably_and_leaves_nothing_dangling` |
| 5 | A dependency is slow or hangs | Every call ends at its deadline (§15.3); a delay changes timing only, never results | the deadline part of `store_unavailable_window_fails_retryably_then_recovers`, `delay_at_every_write_step_only_delays` |
| 6 | The coalescer owner of a ref dies or is replaced | No accepted land is lost or applied twice; a fenced-out owner publishes nothing; another process takes over | `owner_killed_mid_batch_loses_and_duplicates_nothing`, `a_server_on_a_crashed_owners_port_receives_no_forwarded_lands` |
| 7 | The forge database is down | Clone, checkout, fetch and land keep working (§15.1) | `the_vcs_lands_while_the_forge_database_is_down` |
| 8 | Search is down | Change request, check and lease lists and lands keep working | `lists_and_lands_work_while_code_search_is_down` |
| 9 | Stored bytes are corrupted | Corrupt bytes are never served; the read fails with `INTEGRITY`, an alert is raised (§15.6) and fsck reports the object | `corrupted_bytes_raise_integrity_alerts` |
| 10 | Overload, traffic spikes and retry storms | Shedding follows the tiers (anonymous, then bulk) while land and interactive keep reserved capacity; quotas hold per principal and per delegation root; refusals carry `retry-after` | `bulk_is_shed_before_land_and_land_keeps_reserved_capacity`, `rate_limits_are_token_buckets_with_bulk_costs_and_shared_roots` |

Losing a whole cell is classes 3, 4 and 6 at once for that cell, and nothing for the others
(§15.2). A working copy killed at any step of a mutating command is recovered by its journal
(`docs/spec/client-and-cache.md` §4).

## 16. Performance targets

These are **design targets**, not measurements. Phase 1 builds the load harness
(`bee-conformance`, plan task W14) and every target becomes a release gate once measured.
Numbers are per cell on the reference hardware described in `docs/benchmarks.md` (written with W14).

| Operation | Target |
|---|---|
| CreateChangeset, 1 file changed, server time | p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size |
| Land, uncontended, 1-change stack | p50 ≤ 30 ms, p99 ≤ 150 ms |
| Land throughput, one hot ref, landing trains on | ≥ 100 changes/s sustained with 1,000 concurrent landers, no starvation (every lander lands within 10 s p99) |
| Sparse checkout, 1,000 files, warm node cache | ≤ 1 s |
| Sparse checkout, 1,000 files, cold, same region | ≤ 3 s |
| Log / blame / diff via API | p99 ≤ 200 ms for a 100-entry page or one file up to 10k lines |
| Event delivery (commit to subscriber) | p99 ≤ 1 s |
| Concurrent agent sessions | ≥ 10,000 per cell with streams open |
| Availability | reads 99.99%, writes 99.95%, per cell, per month |

Complexity gates (§14.6) are hard requirements: a target may be missed on slow hardware, a
complexity gate may not.

**Production gate.** Before the cloud serves customers, the metadata benchmark (`bench/db`,
workloads H1–H3 plus fault and operations runs) is repeated on at least three machines in separate
zones, with each write synchronously replicated before it is confirmed. The land targets above
must hold with the land coalescer (§7.4) on that setup. The single-laptop results published on
2026-10-18 are evidence for the design, not for production.

## 17. GitHub migration and compatibility

Moving a repo or a whole org from GitHub must be one command, keep every commit id, and allow a
period where both forges stay in sync. Research notes and sources are in
`docs/research/github-migration.md`.

### 17.1 One command

```sh
bee import github acme/api            # one repo
bee import github --org acme          # every repo in an org
bee import github acme/api --dry-run  # fidelity estimate only
```

`bee import github` runs the separately installed `bee-github` importer from the
absolute `BEE_GITHUB_BIN` path when set, or from beside the running `bee` executable.
It never searches `PATH` for the importer.

1. The user installs the Beehive GitHub App on the source org. This gives per-installation rate
   limits and webhook access. A personal token works but is slower.
2. **Dry run:** list what will be carried, archived or dropped (§17.6) and an ETA computed
   from the live rate limit.
3. **Git first:** all branches, tags, `refs/pull/*`, LFS objects and the wiki are fetched over
   git and imported. The repo can be cloned and checked out within minutes, before any
   metadata has arrived.
4. **Metadata second:** the org migration archive (bulk JSON, no API quota cost) is imported
   first; the API fills gaps (force-push events, check runs, review threads) with checkpointed,
   resumable fetches.
5. **Then live sync:** webhooks keep the import current; a periodic reconciliation with `since`
   cursors repairs anything a webhook missed. Re-running the command imports only deltas.

### 17.2 Mapping

| GitHub | Beehive |
|---|---|
| Commits, merge commits | Changesets with identical git ids (§17.4); history is never rewritten |
| Pull request | Change request with the **same number**; ChangeId = BLAKE3(`"github-pr\0" ‖ repo node id ‖ number`)[..16] |
| PR pushes and force-pushes | CR revisions; each force-push's before/after pair is a revision boundary. Pre-force-push commits are fetched while GitHub still serves them; unreachable ones are recorded in the fidelity report |
| PR based on another PR's branch | Stacked CRs (`depends_on`) |
| Reviews, review threads, suggestions | Reviews and comments anchored to the original revision (commit, original line, side, diff hunk); re-projected onto later revisions only where the hunk still matches, otherwise marked outdated |
| Issues, labels, milestones | Imported as issues records (minimal task objects, §11) with the same numbers |
| Releases and assets | Releases |
| Branch protection, rulesets | Land rules in `.beehive/policy.toml`, generated and shown for review |
| CODEOWNERS | Owners in policy (the file is also kept as-is) |
| Users | **Claimable placeholder principals** keyed by GitHub numeric user id; a person claims theirs by logging in with GitHub, and the claim applies to every imported repo at once. Bots become agent principals |
| Check runs, statuses, reactions, Projects, Discussions | Archived read-only records attached to the matching object |
| Secrets, deploy keys, webhooks, app installs | Not exportable from GitHub; a checklist lists names and pre-fills disabled webhooks |

**Git refs.** `refs/heads/<b>` becomes `<b>`, `refs/tags/<t>` becomes `tags/<t>`, `refs/pull/<rest>`
becomes `pull/<rest>` (when pull refs are imported) and any other `refs/<x>` becomes `git/<x>`.
Names §5 rejects are escaped deterministically: bytes that are not UTF-8, controls, space and
`~ ^ : ? * [ \ @` become `%XX`; a dot that starts or ends a component, precedes `lock` at its end
or forms `..` becomes `%2E`; an empty component becomes `%`; an id-shaped name's first `_` becomes
`%5F`; a name over 255 bytes is cut and suffixed with `-` and 8 hex digits of BLAKE3 over the git
name. Unchanged names are assigned before escaped ones, each group by category (branches, tags,
pull refs, other) then bytewise; a taken name gets the same hash suffix, and a second collision
drops the ref. Every rename is listed in the fidelity report. Annotated tags are peeled to their
commit; a tag of a tree or blob is dropped (`target_not_commit`). A commit bee cannot carry with
its git id (limits, forbidden names, integrity failures, a shallow boundary) is dropped with its
descendants (`parent_dropped`) and the refs that point at them (`target_dropped`).

### 17.3 Links keep working

- PR and issue numbers are preserved, and Beehive serves `/<org>/<repo>/pull/<n>` and
  `/<org>/<repo>/issues/<n>` paths, so a rewritten host is the only change in any link.
- `#123` and `github.com/<org>/<repo>/…` links inside imported text are rewritten to Beehive
  links; the original text is kept alongside.
- After cutover the GitHub repo is archived (not deleted) with a pointer to Beehive.

### 17.4 Git compatibility

- **Identical commit ids.** For imported history, each changeset's Meta keeps the original
  commit header bytes (extension key 64: tree line excluded, parents, author, committer,
  encoding, `gpgsig`, extra headers, in order). Git trees are rebuilt deterministically from
  bee trees (git's entry order and mode encoding; non-canonical git modes such as `100664`
  are kept in a per-entry note). So every imported commit re-serializes to its original
  SHA-1 or SHA-256 and signatures stay valid. `git_mapping` (§8) indexes both directions.
- **New history** made in bee gets a deterministic git representation (derived, cached), so
  a git client sees ordinary commits.
- **Object format.** A repo uses one git object format for life: SHA-1 or SHA-256. SHA-1 is
  computed with collision detection; input carrying a cryptanalytic collision attack is
  rejected with `INTEGRITY` (payload `collision`), never given an id.
- **Exact representation rules** (normative; reference implementation `bee-git`):
  - *Blobs* map to blobs with the same bytes. *Trees:* git mode type bits give the bee mode
    (`100xxx` with the owner exec bit → executable, other `100xxx` → file, `120000` symlink,
    `40000` tree, `160000` gitlink); anything else is `INVALID_ARGUMENT`/`INVALID_MODE`.
    Rebuilding a git tree writes one entry per bee entry with git's canonical mode, adds
    the note's hidden entries, and orders entries as git does (bytewise, directories compared
    as if their name ended in `/`).
  - The *git tree note* (tree key 64) is a map `{?1: [[name, mode]…], ?2: [[name, mode,
    note]…], ?3: [name…]}`: key 1 lists visible entries whose git mode digits are not the
    canonical spelling (`100664`, `040000`); key 2 lists git directories that contain no files
    (bee trees cannot be empty below the root), each with its own nested note; key 3 is the
    original entry order, present only when it differs from git's order. Names and modes are
    byte strings, keys 1 and 2 are sorted by name, empty arrays are omitted and an empty note
    is not written. A git tree with two entries of one name, or a name §4.2 forbids, is not
    importable.
  - The *Meta git extension* (Meta key 64) is the commit's header bytes after the `tree` line,
    exactly as found; then, if the commit has a message, the empty line that ends the headers,
    followed by the raw message only when it is not valid UTF-8. Meta `message` is the message
    (lossy UTF-8), `author`/`committer` are `Name <email>` from the identity lines and the
    times come from them (`[0, 0]` when unparseable). The commit is rebuilt as
    `tree <root>\n` ‖ headers ‖ (`\n` ‖ message, with Meta `message` standing in when the
    extension omits it). The importer rebuilds every tree and commit it converts and rejects
    (`INTEGRITY`) any that would not reproduce.
  - The extension is used on export only when the rebuilt commit's id gives back the
    changeset's ChangeId under the §3.3 importer rule and its `parent` lines (duplicates
    dropped) are the git ids of the changeset's parents. An imported changeset that was
    amended or rebased in bee therefore gets the new-history form.
  - *New history:* `tree`, one `parent` per bee parent in order, `author` and `committer`
    (the committer falls back to the author, its time to the author time), an empty line,
    then the message with trailing whitespace removed and a `Change-Id: k_<hex>` trailer: on
    its own line after an empty line, or appended to a final paragraph of `Token: value`
    trailer lines when the message has more than one paragraph; not repeated if the message
    already ends with it. Negative times are written as `0`; offsets are `±hhmm`, clamped to
    ±99:59.
  - *Identity sanitising:* if the text ends with `>` and contains `<`, the email is between the
    last `<` and that `>`, the name is what precedes it; otherwise all of it is the name and
    the email is empty. Both parts lose `<` and `>`, control characters become spaces,
    whitespace runs collapse to one space, and whitespace and `.,:;"'\` are trimmed from the
    ends. An empty name becomes `unknown`. The result passes `git fsck --strict`.
- **Smart HTTP:** Beehive serves `info/refs`, `git-upload-pack` (clone, fetch, shallow,
  partial clone with `blob:none`) and `git-receive-pack`:
  - pushing to `refs/heads/ws/<name>` updates the pusher's workspace ref;
  - pushing to a protected branch is a **Land** of the pushed commits, under the same policy,
    leases and checks as any other land (a rejected land is a rejected push with the reason);
  - pushing a new branch opens or revises a change request when `push.autoChange` is on.
- Packs for clone and fetch are generated from content-addressed objects and cached by
  `(repo, wants, haves)` digest; a clone never touches forge tables (§15.1).

### 17.5 Dual run

| Phase | Source of truth | What syncs |
|---|---|---|
| A. Mirror | GitHub | GitHub → Beehive, live. Agents can work in Beehive; their landed changes are opened back on GitHub as PRs, paced under GitHub's write limits |
| B. Bridge | GitHub for `main`, Beehive for agent work | Two-way: comments, reviews and check statuses bridged; CI keeps running on GitHub Actions or on runners pointed at Beehive refs |
| C. Cutover | Beehive | Beehive → GitHub as a read-only push mirror, then archive |

Each phase is reversible until C, and phase changes are one command (`bee import github … --phase bridge`).

### 17.6 Fidelity report

Every import produces a machine-readable report (`schemas/v1/import-report.schema.json`) and a
summary: per category, how many items were imported, archived and dropped, with the reason
for each dropped item. Categories include unreachable force-push commits, comments with broken
anchors, unclaimed identities, secrets to re-enter, workflow steps that can't run, LFS failures,
and size-limit rejections. "Imported 100%" is never claimed without this report.

The git importer always reports the categories `commits`, `trees`, `blobs`, `refs`,
`annotated_tags`, `submodules` and `lfs_pointers`, lists renamed refs and the LFS objects named by
pointer files (fetched by the forge importer), and records the round-trip verification: every
imported changeset read back from the stores and exported must reproduce its git id and parents,
and every ref its git target. The report's `complete` flag, the only basis for "imported 100%",
is true exactly when nothing was dropped and the requested verification passed. Items left out by
an option (pull refs) are counted as `excluded`, not dropped.

---

## Appendix A: open questions

1. **Connect implementation in Rust**: decided (2026-10-18, §12.1). tonic serves gRPC; the
   HTTP/JSON surface is a Connect-protocol gateway in `bee-proto` (axum) that transcodes
   requests onto the same tonic services, so `@connectrpc/connect-web` clients work unchanged.
   Revisit only if a native Rust Connect server matures; the wire contract stays the same.
2. **Language-aware `comment_only`.** Start with tree-sitter for the top ten languages; other
   files fall back to `logic`.
3. **Rename- and copy-aware merge**: implemented in `bee-merge`; algorithm in
   `docs/contracts/diff3-v1.md`.
4. **Criss-cross merges**: implemented as a recursive virtual merge base; behaviour in
   `docs/contracts/diff3-v1.md`.
5. **Fork networks** (§11.1): decided (2026-10-19): one object namespace per network, per-repo
   changeset and `object_paths` rows (`docs/spec/forks.md`).
6. **Git export cost**: generating packs on demand from changesets vs keeping a per-repo pack
   cache. Start on demand behind the content-addressed cache; measure.
7. **GitHub Actions compatibility**: decided (PRODUCT.md §7.2): `.github/workflows` run unmodified
   on Beehive's own engine and runners, measured against a 500-repo corpus.
8. **Path-partitioned landing on one ref**: not planned. The land coalescer (§7.4) keeps one ref
   far above the land target on one laptop; revisit only if the production gate (§16) fails.

## Appendix B: decision records

Each record: status, date, the decision, why, and what it commits us to. A record is never
edited after it is accepted; a later record supersedes it. One-way doors (Appendix C) need a
record before they change.

### D-1 Typed BLAKE3 ids over one canonical encoding
- **Status:** accepted, 2026-10-17.
- **Decision:** every object id is a kind byte, an algorithm byte and a BLAKE3-256 digest of the
  object's single canonical sCBOR encoding (§3.1, §4).
- **Why:** content addressing must be a pure function of content; one encoding per value makes
  ids reproducible by any implementation, and typed ids make misuse an error.
- **Consequences:** encoding rules are frozen by golden vectors; changing them is a new format
  version with new ids (Appendix C).

### D-2 Changesets record their delta from the first parent
- **Status:** accepted, 2026-10-17.
- **Decision:** a changeset carries the exact delta from its first parent and the resulting root
  tree; Invariant D ties the two (§4.4).
- **Why:** history, blame and classification read deltas without diffing trees, and a one-file
  commit costs O(depth).
- **Consequences:** every writer must produce exact deltas; fsck checks Invariant D.

### D-3 History operations run on the server
- **Status:** accepted, 2026-10-17.
- **Decision:** commit, rebase, merge and land are server RPCs; clients never need full history
  (§7).
- **Why:** agents work through the API without clones, and one implementation decides merges.
- **Consequences:** the server is on the write path; it must be horizontally scalable and every
  write idempotent (§15.5).

### D-4 Lock-free landing through a ref CAS and a per-ref coalescer
- **Status:** accepted, 2026-10-18 (coalescer replaced landing trains).
- **Decision:** land is a compare-and-swap on the ref with a landed index for idempotency; each
  protected ref has one fenced coalescer that publishes queued lands in one CAS (§7.4).
- **Why:** one transaction per land capped a ref at hundreds of lands a second; the coalescer
  reached tens of thousands (bench/db).
- **Consequences:** a lease row per protected ref; forwarded lands name the owner and fence.

### D-5 Two stores with a fixed write ordering
- **Status:** accepted, 2026-10-17.
- **Decision:** metadata in Postgres (SQLite in dev), bytes in an S3-compatible store (disk in
  dev); bytes before rows before refs (§6).
- **Why:** each store does what it is good at, and the ordering means no reader sees a
  reference to bytes that are not durable.
- **Consequences:** garbage collection reconciles the two; fault tests cover every step.

### D-6 Delegated Biscuit tokens
- **Status:** accepted, 2026-10-18.
- **Decision:** tokens are Biscuits that attenuate offline and record the delegation chain;
  every principal of the chain must be allowed (§9.2).
- **Why:** agents act on behalf of humans; scope, budget and audit must follow the chain
  without a round trip.
- **Consequences:** revocation by block id and per-root quotas; key rotation by key id.

### D-7 Policy lives in the repo
- **Status:** accepted, 2026-10-18.
- **Decision:** `.beehive/policy.toml` at the tip of the target ref, overlaid on org defaults,
  governs every action; a policy change takes effect when it lands under the old policy (§9.4).
- **Why:** policy is reviewed and versioned like code, and agents can read it.
- **Consequences:** the server keeps the last good policy when a tip's policy is invalid.

### D-8 Redaction removes bytes, never ids
- **Status:** accepted, 2026-10-17.
- **Decision:** redaction quarantines then purges bytes and blocklists the id; history is never
  rewritten (§13).
- **Why:** rewriting history breaks every clone, link and signature; compliance needs the
  bytes gone, not the history.
- **Consequences:** readers handle `REDACTED`, working copies write a placeholder, purges
  delete content-bearing derived records.

### D-9 Git compatibility with identical commit ids
- **Status:** accepted, 2026-10-17.
- **Decision:** imported commits keep their git headers so they re-serialize to the same SHA,
  and the git gateway serves smart HTTP and SSH (§17.4).
- **Why:** migration must not change a single commit id, and any git client must work.
- **Consequences:** the Meta git extension and the tree note are part of the format.

### D-10 One protobuf schema, gRPC and Connect HTTP/JSON, additive only
- **Status:** accepted, 2026-10-18.
- **Decision:** `proto/bee/v1` is served by tonic and by a Connect gateway over the same
  services; changes are additive, breaking ones go to a new package (§12.1).
- **Why:** typed clients, browsers and curl see one API; old clients keep working.
- **Consequences:** a baseline descriptor test fails any non-additive change.

### D-11 Cells with static stability
- **Status:** accepted, 2026-10-17.
- **Decision:** production is many independent cells; an org lives in one; the directory is
  cached so cells serve while it is down (§15.2).
- **Why:** one fault must not take down every customer (GitHub's 2025-26 incidents).
- **Consequences:** no cross-cell fan-out; rollouts go cell by cell.

### D-12 Forks share the upstream's object namespace
- **Status:** accepted, 2026-10-19.
- **Decision:** a fork's repo lives in its network root's object namespace; forking copies
  rows, never bytes (§11.1).
- **Why:** objects never move between orgs, and a redaction reaches every fork.
- **Consequences:** access decisions per repo over shared objects (`object_paths`).

## Appendix C: one-way-door review

A one-way door is a decision that is expensive or impossible to reverse once data or clients
depend on it. The doors:

| Door | Why it is one-way | Record |
|---|---|---|
| Object encoding and id format (§3, §4) | every stored id and golden vector depends on it | D-1 |
| Delta-from-first-parent and Invariant D (§4.4) | every changeset row encodes it | D-2 |
| Write ordering between the stores (§6.2) | crash safety of every stored repo relies on it | D-5 |
| Wire contract: proto field numbers, types, error reasons (§12) | released clients depend on it | D-10 |
| Token format and delegation semantics (§9.2) | issued tokens and offline attenuation depend on it | D-6 |
| Policy file format version (§9.4) | policies in every repo's history are read with it | D-7 |
| Redaction semantics (§13) | ids and blocklists persist forever | D-8 |
| Git id preservation (§17.4) | imported histories are only correct with it | D-9 |
| Org-then-repo keying and cell placement (§6.6, §15.2) | every row and shard is keyed this way | D-11 |

A change that walks through one of these doors needs, before it merges:

1. a decision record in Appendix B that names the alternatives considered and supersedes the
   record listed above;
2. a compatibility and migration plan: what old data and old clients do, and how they move;
3. new golden vectors next to the frozen ones (frozen vectors are never edited);
4. an entry in `docs/contracts/amendments.md` linking the record.

A test fails the build when a door listed here has no record in Appendix B.
