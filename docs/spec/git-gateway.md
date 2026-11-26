# Git gateway

Status: implemented in `crates/bee-git-http` (layer 6, PLAN task W16), mounted by
`crates/bee-server/src/git_mount.rs` on the server's single listener, with the SSH listener and
key registry in `crates/bee-server/src/git_ssh.rs`. Scope: SPEC §17.4 smart HTTP and SSH: clone,
fetch (thin packs), shallow and partial clone, push mapped to workspace refs and Land (including
from shallow clones), the pack cache, annotated tags, Git LFS and its file locking. Tests:
`crates/bee-git-http/tests/git_cli.rs`, `git_extras.rs` and `git_ssh.rs` drive the real `git`,
`git-lfs` and `ssh` CLIs against an in-process dev server.

## 1. Endpoints

| Route | Use |
|---|---|
| `GET /<owner>/<repo>.git/info/refs?service=git-upload-pack` | Fetch advertisement: protocol v2 capabilities when the request has `Git-Protocol: version=2`, else the v0/v1 ref advertisement |
| `POST /<owner>/<repo>.git/git-upload-pack` | v2 `ls-refs` and `fetch`; v0/v1 stateless-RPC negotiation and pack |
| `GET /<owner>/<repo>.git/info/refs?service=git-receive-pack` | Push advertisement |
| `POST /<owner>/<repo>.git/git-receive-pack` | Push |
| `POST /<owner>/<repo>.git/info/lfs/objects/batch` | LFS batch API |
| `PUT`, `GET /<owner>/<repo>.git/info/lfs/objects/<oid>?size=&exp=&p=&sig=` | Signed LFS transfers |
| `POST`, `GET /<owner>/<repo>.git/info/lfs/locks`, `POST …/locks/verify`, `POST …/locks/<id>/unlock` | LFS file locking (§7) |
| `ssh://git@host:port/<owner>/<repo>.git` | `git-upload-pack`, `git-receive-pack`, `git-lfs-authenticate` over SSH (§8) |

The `.git` suffix is optional. The dumb HTTP protocol is not served (`400 INVALID_ARGUMENT`).
Request bodies may be gzip-encoded (`Content-Encoding: gzip`); the decoded body is limited to
1 GiB.

## 2. Authentication and access

- Credentials: HTTP Basic with the bee token as the password (any user name), or the token as
  the user name with an empty password, or `Authorization: Bearer <token>`. They are verified by
  the server's token path (signature, revocation, expiry, deactivation, rate quotas).
- No credential, or one that does not verify: `401` with `WWW-Authenticate: Basic realm="Beehive"`
  (git then asks its credential helper). An unknown repo: `404`. Other refusals use the HTTP status
  of their SPEC §12.2 reason and a body `<REASON>: <message>`.
- Client side: `bee git credential <get|store|erase>` is a git credential helper that answers
  `get` with the token of `bee login` (or `--token` / `BEE_TOKEN`) for the endpoint's host
  only, and stores nothing. `bee git setup --repo org/name [--remote origin] [--global]` adds the
  remote `<endpoint>/<org>/<name>.git` (no credential in the URL), installs the helper for the
  endpoint (`credential.<endpoint>.helper`, after an empty entry that drops inherited helpers),
  sets `protocol.version=2` and reports `GitPreflight`'s problems for the local git.
- Fetch needs `read` on the repo (the server's repo gate). Git serves whole trees, so a principal
  that may not read every path (token read paths, or a policy access rule) is refused with
  `POLICY_DENIED`; the API serves such principals with filtered results.
- Push needs `commit` on the repo; every pushed changeset then passes the CreateChangeset checks
  (§4) and every ref write its own authorization (Land or RefService rules).
- Refs outside the token's ref scope are not advertised and cannot be pushed.
- Objects are served by id when the repo's `git_mapping` rows name them (`allow-reachable-sha1-in-want`),
  without a ref-scope check: content readability is the whole-tree rule above.

## 3. Ref names

Per principal `me`:

| bee | git |
|---|---|
| `ws/<me>/<rest>` | `refs/heads/ws/<rest>` |
| `ws/<other>/<rest>` | `refs/workspaces/<other>/<rest>` |
| `tags/<t>` | `refs/tags/<t>` |
| `pull/<rest>` | `refs/pull/<rest>` |
| `git/<x>` | `refs/<x>` |
| any other `<b>` | `refs/heads/<b>` |

Every bee name is a valid git name. Pushing a branch whose name would fall in a bee namespace
(`refs/heads/tags/…`, `refs/heads/pull/…`, `refs/heads/git/…`, `refs/heads/ws`) or that the SPEC §5
grammar rejects is `INVALID_ARGUMENT`. `HEAD` is a symref to `refs/heads/main`, or to the first
branch when `main` does not exist (advertised as `unborn` when there is none).

## 4. Git ids (SPEC §17.4)

- The object format is the format of the repo's recorded git ids (from its ref tips' `git_mapping`
  rows), SHA-1 when none is recorded. SHA-256 repos are served in SHA-256 (`object-format=sha256`).
- `bee_git_http::gitmap` applies the exact representation rules of `bee_git::Exporter`
  (differential unit tests compare them): imported commits are rebuilt from Meta's git extension
  only when the result gives back the changeset's ChangeId and parents; everything else gets the
  new-history form with its `Change-Id` trailer. Trees are rebuilt from their children's ids and
  their git note.
- Ids are read from `git_mapping` rows (one row per object); ids derived for new history are
  written back as `git_mapping` rows, so the representation is computed once and a lazy fetch of
  any object finds it.
- An annotated tag pushed to `refs/tags/<t>`, or imported from a git repository, is kept byte-exact
  (`git_tag` record) and linked to the ref by a `git_ref_tag` row (key: `bee_import::ref_tag_key`,
  a foreign id of BLAKE3 over the ref name and the changeset); it is advertised with its exact id and
  its peeled commit and sent for `include-tag`, so a clone's tags have the source's ids and bytes.
  The importer writes the row for every tag ref whose tag object points directly at the commit. Tags
  created through the API, and a tag of a tag (the chain is kept as records), are advertised as
  lightweight tags of their commit.
- A redacted meta or blob cannot be served over git (`REDACTED`): the commit or blob id would change.

## 5. Fetch (upload-pack)

- **Protocol v2** (`ls-refs=unborn`, `fetch=shallow filter`, `server-option`, `object-info`,
  `object-format`). `object-info` answers `size` for each `oid` (an empty size for an object
  the repo lacks or that is redacted). Over HTTP a `git/<version>` user agent older than the
  repo's `min_client_version` (2.30.0 when unset) is refused (`docs/spec/operations.md` §9):
  `ls-refs` with `symrefs`, `peel`, `unborn`, `ref-prefix`; `fetch` with `want`, `have`, `done`,
  `ofs-delta`, `include-tag`, `no-progress`, `thin-pack`,
  `shallow`, `deepen`, `deepen-relative`, `deepen-since`, `deepen-not`, `filter`. Without `done` the
  server acknowledges the haves it has and is `ready` as soon as one is common (else `NAK` and the
  client continues). `packfile-uris` and `want-ref` are not advertised.
- **v0/v1** (`multi_ack_detailed side-band-64k thin-pack ofs-delta shallow deepen-since deepen-not
  deepen-relative no-progress include-tag allow-tip-sha1-in-want allow-reachable-sha1-in-want filter
  symref object-format agent`): each stateless request repeats wants and haves; shallow lines are
  sent in every response of a deepen request; `ACK <id> common` per common have, `ACK <id> ready`
  and `NAK` close a round; `done` gets the final `ACK`/`NAK` and the pack.
- **Selection** (`walk`): commits are painted in generation order (`graph` rows): want-only commits
  are sent; the client's shallow commits are grafts. Deepen requests restrict the want walk to an
  inside set and report its boundary as `shallow`; client shallow commits that become inside with
  their parents are `unshallow`ed and their parents wanted. Trees and blobs are walked against the
  trees of parents the client has or receives, path by path.
- **Filters**: `blob:none`, `blob:limit=<n>[kmg]`, `tree:<depth>`; explicitly wanted objects are
  always sent (lazy fetches). Other filters are `INVALID_ARGUMENT`.
- **Packs** are version 2, whole objects, with offset deltas for blobs whose base at the same path is
  in the same pack (when the client accepts `ofs-delta`, blobs up to 1 MiB).
- **Thin packs** (`thin-pack`): a blob whose base at the same path is in a parent commit the client
  has (a common have) is sent as a ref-delta against that blob, which the client completes
  (`index-pack --fix-thin`). Not used with a filter (a partial clone may lack the base), for a
  redacted base, or above 1 MiB. Without `thin-pack`, packs are self-contained.
- **Pack cache**: plans (pack and shallow lines) are cached in memory, keyed by BLAKE3 over the repo's redaction epoch (a pack built before a redaction is never served after it), the
  repo, wants, common haves, client shallow commits, deepen request, filter, pack options and the
  advertised annotated tags; LRU over 256 MiB, packs over half of it are not cached. A clone of an
  unchanged tip is served from memory. The cache is per process, shared by HTTP and SSH. Every
  `git-upload-pack` HTTP response carries `X-Bee-Pack-Cache: hit|miss|none` (`none`: the request
  planned no pack, such as `ls-refs`).
- **Clone prewarming** (`land.warm_pack`, default on): after each land the server's per-repo
  warming task builds the pack a stock `git clone` asks for and stores it under that request's
  key, so the first clone of the new tip is a cache hit. A stock clone (protocol v2 and v0/v1
  alike) wants every advertised `refs/heads/*` and `refs/tags/*` id (an annotated tag's own id),
  with `ofs-delta` and `thin-pack`, without `include-tag`, haves, shallow, deepen or filter. The
  prewarmed key matches readers whose token sees every ref and who own no workspace (their own
  workspaces show as `refs/heads/ws/…` and change the wants). The cache's limits apply: a pack
  over half the capacity is built and dropped, and nothing is built when the cache is off.
- Errors inside a fetch are sent as an `ERR <REASON>: <message>` packet.

## 6. Push (receive-pack)

Capabilities: `report-status report-status-v2 delete-refs side-band-64k quiet atomic ofs-delta
push-options no-thin object-format agent`. `no-thin` makes clients send self-contained packs.

1. **Unpack**: the pack is parsed and verified (`Pack::parse_with_limit`); a failure is
   `unpack <REASON>: …` and every ref `ng`.
2. **Ingest** (`ingest`): commits the repo lacks become changesets with the importer's rules (Meta
   with the git extension, importer ChangeId, every tree and commit rebuilt and compared:
   `INTEGRITY` otherwise). Subtrees and blobs the client left out are taken from `git_mapping`
   rows, so a push costs O(change). The pusher is Meta `creator` and uploader. Before insertion each
   changeset passes the CreateChangeset checks through the server: write access to every changed
   path, push rules, object availability, push protection (bypass with the push option
   `push-protection-bypass=<false_positive|used_in_tests|will_fix_later>`, allowed and audited per
   `push_protection.bypass`), and the changeset budget. Writes follow SPEC §6.2; the changeset row
   carries `object_paths`, the eager rows and the `git_mapping` rows of the commit and its new trees
   and blobs. Each changeset is audited (`commit`, `{"via": "git"}`).
3. **Protected refs** (policy land rule or ruleset, `main`, `release/*`) move only through Land:
   - the new commit must descend from the client's old value, else `NOT_FAST_FORWARD` (force pushes
     are refused regardless of the `force` permission: Land cannot rewrite history);
   - the stack is landed through the coalescer under policy (approvals, checks, leases, agent
     rules); a refusal is `ng <ref> <REASON>: <message>` with the SPEC §12.2 reason;
   - if the tip moved since the client fetched, Land rebases the stack and the push succeeds; the
     report-status v2 entry carries `option new-oid <id>` so the client's tracking ref names the
     rebased commit. Merge commits in the stack are `MERGE_IN_STACK` (pull with `--rebase`);
   - deleting a protected ref is `POLICY_DENIED`.
4. **Other refs** are a compare-and-swap through `RefService` (create, update, delete) from the
   version read when the push was validated; a client old value that is not the ref's value is
   `VERSION_MISMATCH` ("fetch first"). A non-fast-forward move is sent with
   `allow_non_fast_forward` and needs the `force` permission.
5. **Atomic** (`git push --atomic`): every command is validated before anything is written; at most
   one protected ref may be landed (`INVALID_ARGUMENT` otherwise); plain ref writes go first and are
   rolled back with a compare-and-swap if a later write or the land is refused. The rollback is best
   effort: a ref moved by someone else in between is left alone.
6. Lands and ref writes are audited by the server with action `git-receive-pack:<RPC>`.

Pushes from a **shallow clone** work: the `shallow <oid>` lines a client sends name its boundary
commits, which must be commits the repo has (the pack then needs nothing below them, and the stored
history supplies the parents); a boundary the repo lacks is `INVALID_ARGUMENT` ("this clone is
shallow at …, which the repo does not have").

`push.autoChange` is served by the server's forge wiring (`forge/landing.rs`, called from
`git_mount.rs`): a push that creates a branch other than the default opens a change request when the
repo's `push_auto_change` setting is on or the push carries the option `bee.cr`
(`bee.cr.title=<title>` names it); pushes to a branch with a change request add a revision.

Not served: push certificates. A pushed commit keeps the importer ChangeId even when its message carries a
`Change-Id:` trailer, so its git id stays exact.

## 7. Git LFS

- The batch API accepts `operation` `upload` (needs push access) or `download` (needs fetch access),
  the `basic` transfer and `sha256`. Each object gets a signed `href` on this server valid for one
  hour, `authenticated: true`; an upload of an object already stored gets no action; a missing
  download is a per-object `404`, an oversized upload a per-object `422` (`limits.max_blob_bytes`).
- A signature is HMAC-SHA256 with a key derived from the server's URL key over the method, repo,
  object id, size, expiry and principal. `PUT` verifies the SHA-256 and size, stores the bytes as an
  ordinary bee blob (deduplicated per org, uploaded by the signing principal) and writes an
  `lfs_object` row mapping the LFS id to it. `GET` returns the bytes.
- LFS blobs are referenced by `lfs_object` rows, not by trees; garbage collection treats the blobs
  those rows name as roots (`bee_store::gc::LFS_OBJECT_TYPE`, read with
  `MetaStore::list_derived_by_type`), so an LFS object outlives the upload window.

### Locking

`POST locks` (`{"path": …}`), `GET locks` (`path`, `id`, `cursor`, `limit`), `POST locks/verify`
and `POST locks/<id>/unlock` (`{"force": …}`) follow the git-lfs file-locking protocol, so
`git lfs lock`, `unlock`, `locks [--verify]` and `lfs.locksverify = true` (the push is refused with
"Cannot update locked files" when it changes a file someone else locked) work unmodified.

- Locks are per repo (the protocol's `ref` is accepted and ignored) and keep one lock per path;
  taking a held path is `409` with the holder. A path is relative, `/`-separated and normalized
  (`INVALID_ARGUMENT` otherwise). Ids are 32 hex digits.
- Taking a lock needs `commit` on the repo and write access to the path, as a commit that changes it
  does. `locks` needs `read`; `verify` and `unlock` need `commit`.
- Only the owner unlocks. `force` breaks another principal's lock and needs the `force_update`
  permission on the repo (`PERMISSION_DENIED` otherwise).
- Lock rows live in the server's key-value tables (`lfs_lock`, `lfs_lock_id`), changed atomically.
  `verify` lists the caller's locks as `ours` and everyone else's as `theirs`, paged by path.

## 8. SSH transport

`GitGateway::serve_ssh` (`bee_git_http::ssh`, on the `russh` server library) serves exec requests
on an SSH listener; `bee_server::git_ssh` starts it and keeps the key registry.

- **Commands**: `git-upload-pack '<owner>/<repo>[.git]'`, `git-receive-pack '…'` (the `git
  upload-pack` spelling, a leading `/` and double quotes are accepted) and `git-lfs-authenticate
  '<owner>/<repo>' <download|upload>`, which prints the JSON git-lfs reads (`href` of this server's
  LFS endpoint, an `Authorization: Bearer` header with a one-hour token scoped to the repo,
  `expires_in`). A session without a command (`ssh -T git@host`) prints
  `Hi <name>! You've successfully authenticated, but Beehive does not provide shell access.` on
  stderr and exits 1; any other command is refused with exit 1.
- **Authentication** is public key only; the SSH user name is ignored. The key's `SHA256:`
  fingerprint is looked up in the registry (below) and mapped to a principal; keys nobody registered
  are refused when offered. Every exec opens the repo through the same repo gate as HTTP with a
  token minted for that principal for the session (ten minutes), so token verification (including
  deactivation), the read-everything rule for fetch, `commit` for push, ref scope, policy, push
  protection (with the `push-protection-bypass` push option), Land for protected refs and audit are
  identical. Failures before the protocol starts are an `ERR <REASON>: <message>` packet (git prints
  it as `remote error: …`) and exit status 1.
- **Protocols**: `GIT_PROTOCOL` (sent by git as an environment request) selects v2 (`version=2`), v1
  or v0. The stream is framed by the transport: v2 commands are read to their flush packet; v0/v1
  fetch negotiation re-runs the engine's stateless exchange with the wants and every have so far
  (shallow lines and acknowledgments already sent are dropped from the replay); a push reads its
  commands, push options and exactly the pack that follows (the pack's end is found by walking its
  entries as they arrive, so no EOF is needed). Shallow, partial and thin fetches, tags and the pack
  cache behave as over HTTP.
- **Key registry** (the identity domain's `UserService.AddUserSshKey` is served elsewhere and has no
  key store): `git_ssh::add_key`, `list_keys`, `remove_key` over the server's key-value tables
  (`ssh_key`: fingerprint → record, `ssh_key_user`: principal → fingerprints). A key belongs to one
  active human or service principal (agents hold no SSH keys) and is unique across the server
  (`ALREADY_EXISTS`). Removing a key, or deactivating its principal, ends access at the next
  connection.
- **Deploy keys** (F416-029): a key that is no account's key but matches a repo's deploy key
  (`RepoService.AddDeployKey`, by key material) authenticates too; at exec the server looks the
  key up on the requested repo and opens it with a token for the key's own service principal
  (`deploy-key:<id>`, scoped to that repo: `read`, plus `commit` and `land` unless read-only).
  The repo-access decision gives that principal the key's role (read or write), so a read-only key
  can clone and fetch but `git-receive-pack` is refused, a key never reaches another repo, and
  deleting the key ends access at the next connection. `git-lfs-authenticate` hands out the same
  scoped token.
- **Configuration**: the listener starts with the server when the environment variable
  `BEE_GIT_SSH_LISTEN` names an address (`0.0.0.0:2222`; `127.0.0.1:0` picks a port and logs it).
  The host key is `BEE_GIT_SSH_HOST_KEY` (an OpenSSH private key file) or
  `<data_dir>/ssh_host_ed25519_key`, generated with mode 0600 on first start. User keys: ed25519,
  ECDSA and RSA (SHA-2 signatures).
- **Dependency**: `russh` (pinned, `ring` backend: pure Rust apart from the `ring` primitives already
  in the workspace). It gives a server with public-key auth and exec channels; the alternatives were
  `libssh2` (client library) and an external `sshd` (no principal mapping, not embeddable).

## 9. Not served

- `packfile-uris`, `want-ref` and push certificates.
- A configuration section for the SSH listener (`bee-server`'s `config.rs` belongs to another task,
  so the listener is configured by environment variable) and RPCs that manage SSH keys (the identity
  domain's `AddUserSshKey` and friends are not implemented; §8's registry is their store).
- A tag of a tag is advertised as a lightweight tag of its commit.

## 10. Metrics

The HTTP routes and the SSH listener count into one registry (`bee_git_http::metrics`), which
the server renders at `/metrics` with its own and serves through `OpsService.QueryMetrics` and
`ListMetricDescriptors`:

| Name | Type | Labels |
|---|---|---|
| `bee_git_requests_total` | counter | `transport` (`http`, `ssh`), `service`, `outcome` (`ok`, `error`) |
| `bee_git_request_duration_seconds` | histogram | `transport`, `service` |
| `bee_git_fetch_protocol_total` | counter | `transport`, `version` (`0`, `1`, `2`) |
| `bee_git_upload_pack_bytes_total` | counter | `transport` |
| `bee_git_receive_pack_bytes_total` | counter | `transport` |
| `bee_git_pack_cache_total` | counter | `result` (`hit`, `miss`) |
| `bee_git_push_ref_updates_total` | counter | `result` (`ok`, `rejected`) |
| `bee_git_lfs_bytes_total` | counter | `direction` (`upload`, `download`) |

`service` is `info-refs`, `upload-pack`, `receive-pack`, `lfs-batch`, `lfs-transfer`,
`lfs-locks` or `lfs-authenticate`. An HTTP request is an error when its status is 400 or above or
when it answers with an `ERR` packet; an SSH session when it ends with one.
