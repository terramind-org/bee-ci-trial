# Running Beehive on several servers

Beehive servers keep no state another server needs. Repos, refs, leases, tokens, principals and
the forge's tables live in Postgres; object bytes live in S3. Any number of `bee-server`
processes can serve one deployment from the same Postgres database and the same bucket, behind
any load balancer that speaks HTTP/2 (SPEC D-3).

`deploy/compose.scale.yml` is a working example: three replicas, Postgres 17, an S3 store
(rustfs), a Caddy load balancer and the web app. `scripts/check-scaleout.sh` builds it from the
checkout and proves it under load; its results are at the end of this page.

## How lands stay correct across replicas

Every protected ref (`main` and anything the policy protects) has at most one land coalescer
owner across all replicas (SPEC §7.4). Ownership is a lease row in Postgres with a fencing
token:

- The first replica to land on the ref takes the lease and writes its `server.advertise_url`
  into it. It renews the lease every 2 s (a third of `land.owner_lease_ms`, default 6 s) while it
  has work and gives it up after 30 s idle.
- Every other replica forwards lands of that ref to the owner over gRPC, naming the owner and
  fence it read. The owner refuses a forward that names another process or an older fence.
  The forwarded marker is honoured only with an owner and fence that verify; a marker alone is
  ignored and follows the ordinary protected-ref route.
- If the owner cannot be reached, or the forward fails below the API (the owner died
  mid-request), the forwarding replica lands the change itself with the plain compare-and-swap
  algorithm. The ref's version check and the landed index make every path linearizable: a land
  the dead owner already published is answered `skipped`, never landed twice.
- When the owner's lease expires, the next replica that lands on the ref takes it over with a
  higher fence. A deposed owner checks its fence before every publication and stops.

Non-protected refs (`ws/*`, `green/*` and others) always land by compare-and-swap.

## What every replica must share

Each replica signs things another replica checks. These must be identical on every replica,
and come from the environment in `deploy/scale/server.toml`:

| Setting | Signs | Without it |
|---|---|---|
| `auth.signing_key_hex` | Access tokens | Tokens from one replica fail on the others |
| `blobs.url_key_hex` | Signed blob, CI transfer and media URLs | Each replica generates its own `url.key`; a URL issued by one is refused by the others |
| `auth.oidc_key_pkcs8_hex` | The OpenID provider's id_tokens and JWKS, Actions OIDC tokens, attestations | Each replica generates its own key; the JWKS a client fetches does not verify tokens another replica signed |

Generate them once, keep them in your secret store and give every replica the same values:

The OpenID key must be PKCS#8. With some OpenSSL versions (seen on Ubuntu 24.04),
`openssl genpkey -outform DER` alone produces a key the server refuses as not PKCS#8, so the
command below converts it explicitly.

```sh
openssl rand -hex 32                                    # BEE_SIGNING_KEY_HEX
openssl rand -hex 32                                    # BEE_URL_KEY_HEX
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 | openssl pkcs8 -topk8 -nocrypt -outform DER \
  | xxd -p | tr -d '\n'                                 # BEE_OIDC_KEY_HEX
```

Each replica also needs its own `server.advertise_url`: the address the other replicas reach
it at (`http://server-1:7420` in the compose stack, a pod IP or headless service name on
Kubernetes). Without it a replica advertises the public URL, which is the load balancer, so
forwards cannot reach the owner and every land falls back to compare-and-swap: still correct,
without the coalescer's batching.

What stays private to a replica, in its own `data_dir`: caches, the code search index (each
replica indexes from the store), local work directories for imports, mirrors and workspaces.

## What is per replica

These are counted or cached by each replica on its own. They are bounded, and the bounds are
part of the contract:

- **Admission limits** (`admission.per_principal_rps`, `per_root_rps`): each replica counts its
  own requests, so behind a round-robin balancer a principal can reach about N times the
  configured rate with N replicas.
- **Token revocation** reaches the other replicas within `auth.revocation_refresh_ms`
  (default 1 s).
  A revoked app token can be accepted by another replica for up to this refresh interval.
- **Repo access inputs** (forge repo records, principals' repo roles, repo name resolutions)
  and the **org admin list** in the built-in org default policy are reused for
  `forge.access_cache_ms` (default 1 s) after a change made through another replica.
- **API rulesets** are re-read after 5 s.
- **Signing keys** are re-read after `forge.access_cache_ms` (default 1 s). A rotated key is
  accepted by every replica within that bound; a retired key is refused within the same bound.
  Forge-issued installation, OAuth, agent, workflow-job, and runner tokens also select the
  refreshed active key at signing time, so no replica restart is needed after a rotation.
  The key ring is stored in the shared server metadata KV table. An existing
  `data_dir/signing-keys.json` is imported once when that shared record is first created.

One operation has an additional load-balancer requirement:
- **Git over SSH** is not load balanced in the compose stack: each replica would present its
  own host key. To serve SSH from several replicas, give them all the same host key file
  (`BEE_GIT_SSH_HOST_KEY`) and put a TCP balancer in front of port 2222.

## Postgres connections

A replica holds up to `metadata.max_connections` (default 16) connections for the metadata
store, 4 for the server tables, and up to 4 for each forge surface it has used, which it keeps
open. Postgres's default `max_connections` of 100 is not enough for three replicas: in the first
run of the check, three replicas held 50, 44 and 6 connections a few seconds after starting,
and the third failed with "sorry, too many clients already". `deploy/compose.scale.yml` sets
`max_connections=400` (`BEE_POSTGRES_MAX_CONNECTIONS`). After a full load run of the check
the replicas held 83, 42 and 85 connections (first run) and 89, 56 and 86 (second run); the
replica with fewer had been killed and restarted mid-run. Size a managed database's connection limit for every replica together,
plus the forge's growth as more surfaces are used.

## Schema migrations

Replicas migrate the database themselves when they start, and may start at the same time:
the metadata store, the forge and the server tables each migrate under their own Postgres
advisory lock. The check starts all three replicas at once on an empty database and asserts
none of them restarted.

## The load balancer

`deploy/scale/Caddyfile` is the reference configuration; any balancer works that:

- speaks HTTP/2 to the replicas (cleartext h2c is fine inside a private network), so gRPC passes;
- accepts HTTP/1.1 and HTTP/2 from clients, and streams responses as they are written
  (server-streaming RPCs, git packs);
- takes a replica out while `GET /readyz` fails and puts it back when it answers 200;
- retries only connection failures on another replica, never a request a replica has started.

## Running the stack

```sh
cp deploy/.env.example deploy/.env    # replace every CHANGE-ME, including BEE_URL_KEY_HEX
echo "BEE_OIDC_KEY_HEX=$(openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 | openssl pkcs8 -topk8 -nocrypt -outform DER | xxd -p | tr -d '\n')" >> deploy/.env
docker compose -f deploy/compose.scale.yml --env-file deploy/.env build
docker compose -f deploy/compose.scale.yml --env-file deploy/.env up -d --wait
```

The balancer listens on `BEE_API_PORT` (the API, git smart HTTP and MCP) and the web app on
`BEE_WEB_PORT`. Sign-in and the first admin work as in [self-hosting](self-hosting.md),
with `deploy/scale/server.toml` in place of `deploy/server.toml`; after editing it, recreate
`bee-init` and every replica. `deploy/scale/land-load.py` is the load client the check uses;
point it at your own deployment to measure it (`land-load.py --help`).

## Measured: three replicas, the coalescer owner killed under load

`scripts/check-scaleout.sh` on 2026-11-15, two runs in a row from a clean stack, both passing.
Machine: Apple M4 Pro, 14 cores, 24 GiB, macOS on arm64; Docker 29.6.2 with 14 CPUs and
11.7 GiB. Other builds were running on the machine (load averages below). The images were built
from commit `fa6d7af6` of this branch.

What each run does: three replicas start at once on an empty database; the first admin signs in
through the balancer; the balancer is shown to pass HTTP/JSON (nine calls answered three times
by each replica), gRPC (`ResolveRef` and a `Land` with grpcurl) and git smart HTTP (clone and
push); every replica publishes the same JWKS. Then server-2 is made the coalescer owner of
`main`, 32 clients land 1,600 files through the balancer, each its own file and changeset
created on the tip `main` had when the run began, and server-2 is `docker kill`ed when 640 lands
are acknowledged and started again 10 s later, longer than the 6 s owner lease.

| | Run 1 | Run 2 |
|---|---|---|
| Lands acknowledged | 1,600 of 1,600 | 1,600 of 1,600 |
| Load phase | 15.8 s | 18.0 s |
| Throughput | 101.4 lands/s | 88.9 lands/s |
| Land RPC p50 / p99 / max | 165.8 / 1,135.7 / 1,503.5 ms | 200.5 / 1,234.2 / 1,642.5 ms |
| Upload + create + land per file, p50 / p99 | 234.8 / 1,289.2 ms | 277.7 / 1,338.4 ms |
| Land answered by server-1 / server-2 / server-3 | 689 / 215 / 696 | 688 / 236 / 676 |
| Requests retried by the client (HTTP 502 from the balancer while server-2 died) | 10 | 10 |
| Lands answered `skipped` on retry | 0 | 0 |
| server-2 killed / started | 5.3 s / 15.4 s into the run | 5.8 s / 16.0 s into the run |
| `main` afterwards | version 1608, 1,608 changesets | version 1608, 1,608 changesets |
| Lease of `main` before → after | server-2, fence 2 → server-1, fence 3 | server-2, fence 2 → server-3, fence 3 |
| Load average before / after the load | 7.82 / 10.39 | 5.95 / 10.17 |
| Whole check | 97 s | 90 s |

Both runs asserted: every acknowledged land's change is in the history of `main` exactly once,
at the changeset `Land` answered; history is linear; all 1,600 files are in the tip's tree with
their blobs; server-1, server-2 and server-3, each asked directly with `min_version` 1608,
report the same tip; a `git clone` through the balancer has 1,608 commits, the same count as
history, and every landed file; the lease moved to a surviving replica with a higher fence; no
replica restarted on start-up.

Time to land includes waiting in the owner's queue: 32 clients land on one ref, which the
coalescer publishes in batches (in run 1, server-1 published 135 batches and server-3 96,
counting the lands they made directly while server-2 was dead; server-2's counters restarted
with it). These numbers come from a laptop running other builds at the same time; they show the
behaviour, not a capacity limit.

### What the first runs found

The stack did not pass at first. Each failure was a real multi-replica bug, now fixed with a
test that fails without the fix:

- **Lands failed when the owner died.** With server-2 killed as the owner, 16 of 640 lands
  failed with `INTERNAL` "transport error": replicas forwarding to it got a reset stream, which
  was only treated as "owner unreachable" when it said `UNAVAILABLE`. A forward that fails
  without a Bee error detail now lands directly (`coalescer.rs`;
  `tests/coalescer.rs::a_forward_that_fails_below_the_api_lands_directly`).
- **A replica crashed on start-up.** Three replicas creating the server tables at once raced in
  Postgres's catalogs ("duplicate key value violates unique constraint
  pg_type_typname_nsp_index"); one exited and was restarted. The tables are now created under an
  advisory lock (`kv.rs`; `pg_replicas_starting_at_once_all_open_the_store`, run with
  `BEE_TEST_PG`).
- **The JWKS differed between replicas.** Each replica generated its own OpenID signing key, so
  two of the three published a key the balancer's JWKS did not match. `auth.oidc_key_pkcs8_hex`
  shares one (`oidc.rs`; `the_configured_oidc_key_is_the_published_one`).
- **Postgres ran out of connections** with three replicas and the default limit of 100 (see
  above).
- **Org admin changes stayed on one replica** (found reading the code while writing this page):
  the built-in org default policy embeds the org admin list, and a replica re-read it only when a
  change was made through that same replica, so a removed org admin kept policy admin rights on
  the others until they restarted. It is now re-read after `forge.access_cache_ms`
  (`tests/access.rs::org_admin_changes_reach_other_processes_within_the_access_bound`).
