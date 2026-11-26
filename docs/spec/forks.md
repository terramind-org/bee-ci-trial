# Forks (`bee-forge::forks`)

Fork networks, fork sync, change requests from a fork to its upstream, maintainers' edits,
outside-contributor CI approval and the visibility rules of private forks. Contract: SPEC
§6.1, §9.3, §11.1, §13; PRODUCT §4.16 (forks, sync, network) and §4.14 (fork CI approval).
Conventions: `docs/spec/forge-core.md`.

## 1. Layout

| path | owns |
|---|---|
| `forks/mod.rs` | `MODULE` (surface `forks`, tables, migration `forks/0001_create_forks`, event kinds, audit prefix `fork.`), `Forks`, `ForkAccess` |
| `forks/vcs.rs` | `ForkVcs` (what forks need from the VCS), `HistoryForkVcs` over a `Writer`, `copy_changesets`, `vcs_namespace`, `vcs_name` |
| `forks/gate.rs` | `ModerationGate`: CI's `ForkRunGate` over enterprise moderation |
| `bee-server/src/forge/forks.rs` | `ServerForkVcs`: the port over the server's stores, ref moves through the write path |

`Forks::attach` installs the module in the forge (`Forge::forks`), adds `ForkAccess` as an
access source and listens for `ref.updated` to follow fork refs. `RepoService.ForkRepo`,
`ListForks` and `SyncFork` answer `UNIMPLEMENTED` until it is attached.

## 2. Storage: one object namespace per network

Objects are stored once per org (SPEC §6.1) and never move between orgs. A fork therefore
does not get a namespace of its own: its VCS repo is created **in the namespace (VCS org) of
its upstream**, which is the namespace of the network root, named `<name>~<fork id>` (VCS
names are unique per namespace). Its forge repo row is an ordinary repo of the destination
org with `fork_parent_id` set; the server resolves `org/name` of a fork through the forge.

Forking copies **rows, not objects**: for each copied ref, every changeset reachable from
its tip is rebuilt in the fork from its parents, meta, ChangeId and delta (`copy_changesets`).
The rebuild must reproduce the original id (else `INTERNAL`); it inserts the fork's changeset,
parent-edge and `object_paths` rows. Trees and blobs already have their rows and bytes in the
namespace, so no object row is added and no blob byte is written
(`tests/forks.rs::outside_contributor_forks_opens_cr_waits_for_ci_approval_and_lands` counts
both).

Consequences:

- **Isolation.** Readability is per repo through `object_paths` (SPEC §9.3). Content
  committed only in the fork has rows only in the fork, so it is never readable through the
  upstream, and upstream content added after the fork is not readable through the fork until
  a sync copies it.
- **Redaction** is per namespace (`redactions` is keyed by org): redacting an object in any
  member purges it everywhere and bumps the redaction epoch of every member that references
  it.
- **GC** roots are per namespace, so fork refs keep shared objects alive.
- **Forge reads** of history (`HistoryVcs`, `HistorySecurityVcs`) find the namespace with
  `vcs_namespace`: the repo's org, or for a fork the org on its VCS repo row.

`FORKS` (`forge_forks`): global scope, id = fork repo id, `k1` = parent id, `k2` = network
root id, `n1` = creation time, body `ForkRecord`. `FORK_CHANGES` (`forge_fork_changes`):
scope = fork repo id, id = change request id, `k1` = the fork ref it follows.

## 3. Forking

`ForkRepo { repo, org, name, default_ref_only }`:

| Rule | Answer |
|---|---|
| caller cannot read the upstream | `NOT_FOUND` |
| upstream setting `allow_forking` is false | `PERMISSION_DENIED` |
| upstream is private or internal, and its org neither sets `members_can_fork_private_repos` nor has the caller as owner or admin | `PERMISSION_DENIED` |
| caller may not create repos (or public repos) in the destination | `PERMISSION_DENIED` |
| destination name taken | `ALREADY_EXISTS` |
| the destination org already has a fork of this upstream | that fork (as GitHub) |

- `org` empty = the caller's personal namespace (the org named after their first login).
- `name` empty = the upstream's name. Description, topics, homepage and default ref follow
  the upstream.
- Refs: the default ref only, or every branch and tag (not `pull/`, `git/` or workspace refs).
- Visibility: public upstreams fork public, private and internal ones fork private. A fork's
  visibility cannot be changed (`UpdateRepo` answers `INVALID_ARGUMENT`, subcode
  `fork_visibility`).
- Emits `repo.forked` on the upstream (GitHub `fork` webhook) and audits `fork.create`.

**Private forks.** A private fork is readable by the members and collaborators of its own
org, and (`ForkAccess`) by principals holding `maintain` or `admin` on its parent, as GitHub
gives the upstream's owners visibility of private forks. Nobody else sees it.

**Network.** `ListForks` lists the live direct forks the caller may read. `Forks::network`
lists every live member (root first); a fork of a fork joins its parent's network.

## 4. Sync

`SyncFork { repo: fork, ref }` (empty ref = the fork's default ref, synced with the upstream's
default ref; another ref syncs with the upstream ref of the same name). Needs `commit` on the
fork ref and read on the upstream.

- Upstream changesets the fork lacks are copied (rows only); `changesets_pulled` counts them.
- Fork ref behind: fast-forward. Fork ref already containing the upstream tip: unchanged.
- Diverged: the fork's own stack is rebased onto the upstream tip; conflicts answer
  `MERGE_CONFLICT` with the `ConflictSet` and leave the ref as it was.
- The ref moves through the write path (ref events, CI, webhooks); emits `fork.synced` and
  audits `fork.sync`.
- Not a fork: `INVALID_ARGUMENT` (`not_a_fork`). Upstream deleted: `INVALID_STATE`.

## 5. Change requests from a fork

`OpenChangeRequest` gains `head_repo` (the fork holding `head`), `head_ref` (the ref the CR
comes from) and `allow_maintainer_edits`; `ChangeRequest` records `head_repo_id`, `head_ref`
and `allow_maintainer_edits` (additive, `docs/contracts/amendments.md`).

- **Authorization.** Read on the target repo (outside contributors open CRs from their forks),
  `commit` on the fork's `head_ref`, both repos in one network (`INVALID_ARGUMENT`,
  `not_in_network`), plus moderation's write guard on the target.
- **Publication.** At open and at every revision the head's stack is copied into the target
  (rows only), so review, classification, checks and CI run on the target exactly as for a
  native CR, and the land coalescer lands it like any other stack. Opening the CR is the
  fork writer's act of publishing those changesets to the upstream (GitHub's
  `refs/pull/<n>/head`); content that is in no CR stays fork-only.
- **Following the fork ref.** A push to `head_ref` in the fork revises the CR (as its author).
- **Author rights.** The author edits and revises their fork CR with read access on the
  target; only the author may set `allow_maintainer_edits` (`EditChangeRequest`).
- **Maintainers' edits.** A target writer may revise a fork CR only when
  `allow_maintainer_edits` is set (else `PERMISSION_DENIED`); the new head (in the target) is
  copied into the fork and the fork's `head_ref` moves to it.
- **Landing** is unchanged: the target's land rules, required checks and approvals decide.

## 6. CI for fork change requests

A run triggered by a CR whose `head_repo_id` is another repo is a fork run
(`TriggerContext.from_fork`, `pull_request.head.repo.fork`):

- **Approval.** Under the actions policy's `fork_approval`: `ALL_EXTERNAL` holds every fork run;
  `FIRST_TIME_CONTRIBUTORS` and `FIRST_TIME_NEW_ACCOUNTS` ask CI's `ForkRunGate`. The server
  installs `ModerationGate`, which runs moderation's first-time contributor decision (dry run):
  its `run_approval` setting (repo, else org; default `first_time`) decides, a first-time
  contributor being one with no collaborator standing and no landed CR in the repo. A held run
  is `ACTION_REQUIRED` until `ApproveWorkflowRun` by a writer; without a gate, fork runs under a
  first-time policy wait.
- **Secrets.** Jobs of fork runs redeem no secrets unless the policy's `fork_send_secrets` is
  set.

## 7. Deletion

- Deleting the upstream (soft delete) leaves its forks working with their history (the shared
  namespace stays); syncing answers `INVALID_STATE` until the upstream is restored.
- A deleted fork drops out of `ListForks` and the network; change requests it opened keep their
  copied stacks and can still land; pushes no longer revise them.

## 8. Server wiring

- `ServerForkVcs` creates the fork's VCS repo with `bee_server::server::create_repo` in the
  upstream's namespace, copies with `copy_changesets` through `AppState::history` (eager derived
  rows included) and moves refs with the same `ref.updated` VCS event as `RefService`, then
  wakes the event bridge.
- `AppState::repo` resolves a fork's `org/name` through the forge and reports the forge's full
  name; the policy of a fork uses the fork's org default policy; bridged fork events carry the
  fork's forge org.

## 9. API gaps

- No `CompareUpstream` RPC: ahead/behind counts are not served (the domain has `network`,
  `root_of` and `sync`).
- Repo `forks` counts are not maintained on `Repo.forks`.
- A fork inherits the upstream's `.beehive/policy.toml`; its `[members]` apply in the fork
  until the fork's owner lands a change to it.
- Removing a user's access to a private upstream does not delete their private forks, and making
  a public upstream private does not detach its public forks.

## 10. Tests

`crates/bee-forge/tests/forks.rs` (domain, real history, local CI runner, moderation) and
`crates/bee-server/tests/forks_compat.rs` (real server, the GitHub gateway, `gh` opt-in).
