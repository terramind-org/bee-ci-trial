# Self-hosting Beehive

This guide runs your own Beehive: the server, the web app, Postgres and S3-compatible storage,
on one machine with Docker Compose. A self-hosted instance runs the **enterprise** edition.
Every configuration key is defined in [`docs/spec/server.md`](spec/server.md) §2; this guide
says which ones to set and why, and does not repeat the tables.

## Requirements

- Docker Engine 24 or newer with the Compose plugin (`docker compose version`).
- 2 CPUs and 4 GiB of memory for a small team. The first image build compiles the Rust
  workspace: give Docker 8 GiB of memory and expect 10 to 20 minutes.
- For production: a DNS name pointing at the host, and ports 80 and 443 (TLS), plus 2222 if you
  want git over SSH.

## Quick start (development stack)

`deploy/compose.dev.yml` runs one server in `--dev` mode (SQLite and disk, no external service)
and the web app. It is for trying Beehive out, not for real data.

The dev runner executes workflow steps as the server's OS user with that user's file and network
access; it is not a sandbox. Any repository on the server with a workflow, including an imported
or mirrored repository, can run steps on this machine, so use it only with repositories you trust.
Set `[forge.local_runner] enabled = false` to turn it off under `--dev`; production uses registered
runners.

```sh
docker compose -f deploy/compose.dev.yml up --build
```

When both containers are healthy:

- the web app is at <http://localhost:3100>
- the server (gRPC, HTTP/JSON, MCP, health) is at <http://localhost:7420>

The host ports come from `BEE_API_PORT`, `BEE_WEB_PORT` and `BEE_SSH_PORT` (defaults
7420, 3100 and 2222) when something else already listens there:
`BEE_API_PORT=17420 docker compose -f deploy/compose.dev.yml up --build`.

Dev mode creates org `dev`, user `dev`, repo `dev/playground` and a second human, `reviewer`,
and writes their tokens to `/data/credentials` in the `bee-dev-data` volume:

```sh
docker compose -f deploy/compose.dev.yml exec bee-server cat /data/credentials
```

```toml
endpoint = "http://127.0.0.1:7420"
token = "..."
reviewer = "..."
reviewer_token = "..."
```

Open the web app, choose sign in, and paste the `token` value. `docker compose -f
deploy/compose.dev.yml down --volumes` deletes everything.

## Production stack, step by step

`deploy/compose.yml` runs:

| Service | Role |
|---|---|
| `postgres` | Metadata and forge tables (Postgres 17). |
| `s3`, `s3-init` | S3-compatible blob store, and a one-shot job that creates the bucket. Swap in any S3 service by deleting both and setting the `BEE_S3_*` variables. |
| `bee-init` | One-shot `bee-server --check-config` against `deploy/server.toml`. A bad config stops the stack before the server starts. |
| `bee-server` | The server. Starts after Postgres is healthy and the two one-shots succeeded. |
| `web` | The web app, talking to `http://bee-server:7420`. |
| `caddy` (profile) | TLS in front of both, optional. |

1. **Create the environment file.**

   ```sh
   cp deploy/.env.example deploy/.env
   ```

   Replace every value marked `CHANGE-ME`. Generate the token signing key and the URL signing key with `openssl rand -hex 32`, once each.
   Keep the Postgres password URL-safe and identical in `POSTGRES_PASSWORD`,
   `BEE_POSTGRES_URL` and `BEE_FORGE_DATABASE`. Set `BEE_PUBLIC_URL` to the address
   agents, the CLI and git use to reach the server. Do not commit `deploy/.env`.

   The host ports are variables too: `BEE_API_PORT` (server, default 7420, published on
   `BEE_API_BIND`, loopback by default), `BEE_WEB_PORT` (web app, 3100),
   `BEE_SSH_PORT` (git over SSH, 2222) and, for the caddy profile, `BEE_HTTP_PORT` and
   `BEE_HTTPS_PORT` (80 and 443).

2. **Review `deploy/server.toml`.** It is the production configuration: Postgres metadata, S3
   blobs in the S3 store, a forge database in its own Postgres schema, backups to the S3 store, and
   `$VAR` values read from the environment. Secrets stay in `deploy/.env`.

3. **Build and start.**

   ```sh
   docker compose -f deploy/compose.yml build
   docker compose -f deploy/compose.yml up -d
   docker compose -f deploy/compose.yml ps
   ```

   Compose reads `deploy/.env` because it sits next to the compose file. Both images take a
   `BUILD_SHA` build argument that ends up in the image labels and the web environment:
   `docker compose -f deploy/compose.yml build --build-arg BUILD_SHA=$(git rev-parse --short HEAD)`.

4. **Check it.**

   ```sh
   curl -fsS http://localhost:7420/healthz
   curl -fsS http://localhost:7420/readyz
   curl -fsS -o /dev/null -w '%{http_code}\n' http://localhost:3100/
   ```

   The server port is published on the loopback interface only, so the CLI and git work from
   the host. Other machines reach the instance through Caddy (or your own proxy) on 443; set
   `BEE_API_BIND=0.0.0.0` to publish the server port itself.

5. **Sign in and become admin**, as described in [First admin and SSO](#first-admin-and-sso).

## Configuration

The server reads one TOML file. Unknown keys are errors, and a string that is exactly `$NAME`
is replaced by the environment variable `NAME`. The sections you will touch:

| Section | What to decide | Reference |
|---|---|---|
| `[server]` | `public_url` (the address clients use), `listen`, `data_dir`, `trust_forwarded_for` behind a proxy, `edition`. | server.md §2 |
| `[metadata]` | Postgres connection, schema, pool size, optional read replica. | server.md §2 |
| `[blobs]` | S3 bucket, endpoint (path-style when set), credentials. | server.md §2 |
| `[auth]` | The token signing key. Services that log in with a secret (`[[auth.services]]`). | server.md §2 |
| `[forge]` | Forge database, `site_admins`, agent runner, Pages domain. | server.md §2 |
| `[login]` | GitHub device flow, OIDC issuer, default org. | server.md §2 |
| `[import]` | Directories local-path imports may read. | server.md §2 |
| `[backup]` | Backup target and retention. | operations.md §4 |
| `[admission]`, `[limits]` | Rate quotas, request and blob size limits. | server.md §2 |
| `[tls]` | TLS on the server's own listener, client certificates. | server.md §2 |
| `[tracing]`, `[logging]` | OTLP export, log level. | server.md §2 |

Run `bee-server --config <file> --check-config` after every edit. In compose,
`docker compose -f deploy/compose.yml run --rm bee-init` does it with your real environment.

### Edition

`[server] edition` is `"enterprise"` (the default, and what `deploy/server.toml` sets) or
`"cloud"`. Enterprise is the self-hosted edition: an instance landing page, no pricing or
billing screens, and the admin console. `"cloud"` turns on the hosted-service surfaces (marketing
landing, self-serve sign-up, pricing, organisation billing) and is meant for the hosted service
only; do not set it on a self-hosted instance. Any other value fails `--check-config` with the
reason `invalid_config`. The web app reads the edition from the server's `GetServerInfo`.

### Data and volumes

| Volume | Holds |
|---|---|
| `postgres-data` | Metadata and forge tables. |
| `s3-data` | Blob objects. |
| `bee-data` | `/data`: the generated SSH host key, import staging, key material, and restored data directories under `/data/restores/<cell>`. The signing key lives in your `.env`. |
| `caddy-data`, `caddy-config` | Certificates and Caddy state. |

## TLS

Pick one.

**Caddy (recommended).** Set `BEE_DOMAIN` and `BEE_PUBLIC_URL=https://<domain>` in
`deploy/.env`, point the DNS record at the host, then:

```sh
docker compose -f deploy/compose.yml --profile caddy up -d
```

`deploy/Caddyfile` sends API, MCP, blob, health, webhook, sign-in (`/login/oauth/*`,
`/login/device/*`, `/.well-known/*`, `/saml/*`) and git requests to the server and everything
else to the web app, and Caddy obtains and renews the certificate. `trust_forwarded_for = true` in
`deploy/server.toml` is what makes client addresses come from the proxy. With Caddy in place you can
remove the `ports` entry of the `web` service so the web app is reachable only through TLS.

**The server's own `[tls]`.** Set `tls.cert_file` and `tls.key_file` (PEM) and mount them into the
container. The listener then speaks TLS only, and `client_ca_file` enables mutual TLS. Use this when
you have no proxy; keep `trust_forwarded_for = false`.

## Git over SSH

Set two environment variables on the server container (compose already does):

| Variable | Meaning |
|---|---|
| `BEE_GIT_SSH_LISTEN` | Address of the SSH listener, `0.0.0.0:2222`. Unset or empty disables it. |
| `BEE_GIT_SSH_HOST_KEY` | OpenSSH private key file for the host identity. Default: `<data_dir>/ssh_host_ed25519_key`, generated with mode 0600 on first start, so the identity survives restarts because `/data` is a volume. |

Publish port 2222 (`BEE_SSH_PORT` in `deploy/.env`). Users register public keys with
`UserService.AddUserSshKey`. Back up the host key with `/data`, or clients see a changed-host-key warning after a restore.

## First admin and SSO

Principal ids are generated when someone first signs in, so the first admin is a two-step
process.

1. Configure how people sign in: uncomment one `[login]` block at the end of
   `deploy/server.toml` and fill it in: `login.github_client_id` (GitHub device flow), or
   `login.oidc_issuer` and `login.oidc_client_id` (any OIDC provider), plus
   `login.default_org` if new principals should join an org. Then recreate the server:

   ```sh
   docker compose -f deploy/compose.yml up -d --force-recreate --no-deps bee-init bee-server
   ```

   `server.toml` is bind-mounted as a single file. Editors that save by writing a new file
   (vim, most IDEs) leave the running container on the old copy; recreating the container, as
   above, always picks up the edit.
2. Sign in once at `/` (or with `bee login`), then read your principal id from
   `AuthService.WhoAmI`:

   ```sh
   curl -s -X POST "$BEE_ENDPOINT/bee.v1.AuthService/WhoAmI" \
     -H 'Content-Type: application/json' -H "Authorization: Bearer $BEE_TOKEN" -d '{}'
   ```

   Put the id in `forge.site_admins` and recreate the server as in step 1:

   ```toml
   [forge]
   site_admins = ["<your principal id>"]
   ```

Site admins are also operators. Sign in again and open **`/admin`**, the admin console, for
server-wide operator tasks.

## Backup and restore

The server backs itself up (`AdminService.CreateBackup`, operators only). With
`backup.target = "s3"` (what `deploy/server.toml` sets) backups land in the same bucket under
`backup.s3_prefix`. A backup holds:

- every Postgres table of the metadata and forge schemas, taken in one repeatable-read
  transaction (no `pg_dump` needed),
- the key material in `/data` (`signing-keys.json`, `url.key`), so tokens keep verifying,
- per-org blob manifests. With S3 the bucket is the durable copy of the blobs, so **protect the
  bucket** (replication or versioning on the storage side). Backups contain key material too.

Take one and watch it finish (`$BEE_TOKEN` is a site admin's token):

```sh
rpc() { curl -fsS -X POST "http://localhost:7420/bee.v1.$1" -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $BEE_TOKEN" -d "$2"; }
rpc AdminService/CreateBackup '{"note":"nightly"}'     # returns {"id": "<backup id>", ...}
rpc AdminService/ListBackups '{}'                      # state BACKUP_STATE_SUCCEEDED when done
```

**Drill.** Restore into scratch storage on the running instance and check it, without touching
live data: `rpc AdminService/StartRestore '{"backupId":"<id>","targetCell":"drill1","drill":true}'`,
then poll `rpc AdminService/GetRestore '{"id":"<restore id>"}'` until `state` is no longer
`BACKUP_STATE_RUNNING`. A good drill has `fsckClean` and `conformancePassed` true. It writes the
Postgres schemas `restore_drill1` and `restore_drill1_forge` and the directory
`/data/restores/drill1`; drop them when you are done.

**Restoring after losing the database or the data volume.** Keep `deploy/.env` (above all
`BEE_SIGNING_KEY_HEX`) and `deploy/server.toml`; the bucket must have survived.

1. Start a fresh stack: `docker compose -f deploy/compose.yml up -d`. Your existing site admin
   token still works, because the signing key and `forge.site_admins` are in your configuration.
   `ListBackups` shows the backups it finds in the bucket.
2. Restore one into a new cell:
   `rpc AdminService/StartRestore '{"backupId":"<id>","targetCell":"recovered","drill":true}'`
   and poll `GetRestore` until it succeeds with `fsckClean` and `conformancePassed`.
3. Point the server at the restored data. In `deploy/server.toml`:

   ```toml
   [server]
   data_dir = "/data/restores/recovered"
   [metadata]
   postgres_schema = "restore_recovered"
   ```

   and in `deploy/.env`, the forge schema: `BEE_FORGE_DATABASE=...?schema=restore_recovered_forge`.
4. Recreate the server and the web app:
   `docker compose -f deploy/compose.yml up -d --force-recreate --no-deps bee-init bee-server web`.
   Check `/readyz`, run `AdminService/RunFsck` per org, and clone a repo.

The backup covers what was committed when it started; work landed after it is lost. Point-in-time
recovery between backups needs WAL archiving, which the server does not keep. Details and
retention: [`docs/spec/operations.md`](spec/operations.md) §4. `scripts/check-selfhost.sh` runs
exactly these steps (see [Verified](#verified)); test a restore before you need one.

## Checking a production configuration

```sh
docker compose -f deploy/compose.yml run --rm --no-deps bee-server --config /etc/bee/server.toml --check-production
```

It prints the checklist and exits 1 when an error-level item fails. Warnings do not block.

| Item | Level | A failure means |
|---|---|---|
| `metadata_postgres` | error | `metadata.backend` is not `postgres`. SQLite serves one process on one machine. |
| `metadata_replica` | warning | No `metadata.postgres_replica_url`; reads share the primary and stop with it. |
| `stale_reads` | warning | `metadata.stale_read_max_ms` is 0; ref reads fail during a primary outage. |
| `blobs_s3` | error | `blobs.backend` is not `s3`; blobs sit on one machine. |
| `tls` | error | Neither `tls.cert_file` is set nor an `https://` `public_url` with `trust_forwarded_for`. |
| `signing_key` | error | `auth.signing_key_hex` is empty; each process would generate its own key. |
| `url_key` | error | Local signed URLs without a shared `blobs.url_key_hex` (not an issue with S3). |
| `operators` | error | No `ops.operators` and no `forge.site_admins`. Do the [first admin](#first-admin-and-sso) steps. |
| `rate_limits` | error | `admission.per_principal_rps` or `per_root_rps` is 0. |
| `import_roots` | error | `import.allowed_roots` contains `/`. |
| `backups` | error | `backup.target` is neither `s3` nor a directory outside `data_dir`. |
| `logging` | warning | `logging.level` is `debug`. |

On a fresh install `operators` fails until you add the first site admin, and `tls` fails until
`BEE_PUBLIC_URL` is an `https://` address. That is expected.

## Upgrades

1. Take a backup (above) and wait for it to succeed. Read the release notes for schema changes.
2. Get the new version: pull the source and rebuild
   (`docker compose -f deploy/compose.yml build --build-arg BUILD_SHA=$(git rev-parse --short HEAD)`),
   or set `BEE_SERVER_IMAGE` and `BEE_WEB_IMAGE` in `deploy/.env` to the new tags and
   `docker compose -f deploy/compose.yml pull`.
3. `docker compose -f deploy/compose.yml up -d`. `bee-init` validates your configuration
   against the new binary before the server is replaced (a config the new version rejects leaves
   the old server running), and the server runs its schema migrations at start.
4. Check `/readyz`, the web app, and `docker compose -f deploy/compose.yml ps`.
5. To roll back, set the previous image tags and `up -d` again. If the new version migrated the
   schema, restore the backup from step 1 instead (see Backup and restore).

**Rolling upgrade.** The server is stateless apart from Postgres and S3, so you can run several
`bee-server` processes behind one load balancer (they share `auth.signing_key_hex`, Postgres and
the bucket; set `server.advertise_url` per process so lands are forwarded to the coalescer owner).
Upgrade them one at a time: replace a process, wait for `/readyz`, move on. Check the release notes
for schema changes before you run two versions side by side. Compose runs a single server;
run several under an orchestrator that can replace them one at a time.

## Observability

| Surface | Use |
|---|---|
| `GET /healthz` | Liveness: the process answers. The image's `HEALTHCHECK` calls it. |
| `GET /readyz` | Readiness: metadata and key-value stores answer within 2 s; 503 otherwise. Use it for load balancers. |
| `GET /metrics` | Prometheus text with fixed metric names. |
| `[tracing]` | `otlp_endpoint` (OTLP/HTTP collector base URL), `sample_ratio`, `service_name`, `export_interval_ms`. Spans go to `<url>/v1/traces`. |
| `[logging]` | Structured JSON logs; `requests = true` logs one line per request. |

Do not expose `/metrics` to the internet. The provided Caddyfile does not forward it: scrape
`bee-server:7420/metrics` on the compose network, or `127.0.0.1:7420/metrics` on the host.

## Agent fleets: the local daemon and cache tiers

Agents on many machines read the same repos again and again. The image ships `bee-daemon`, a
local cache that sits between agents and the server. Run it next to the agents:

```sh
BEE_ENDPOINT=https://bee.example.com BEE_TOKEN=<token> bee-daemon serve
```

Configuration is one `[daemon]` table (`--config` or `BEE_DAEMON_CONFIG`): `upstream`,
`cache_dir`, `max_bytes`, `bootstrap` (repos to warm) and more. `bee-daemon regional` runs a shared
cache for a region or a rack. The full key list and the tier model are in
[`docs/spec/cache-tiers.md`](spec/cache-tiers.md) §1 and §11.

## Pointing agents at the instance

```sh
export BEE_ENDPOINT=https://bee.example.com
export BEE_TOKEN=<token>               # a service or agent token
bee git setup --repo acme/api          # adds the remote and the credential helper
git fetch origin
```

- **CLI:** `bee login` saves the endpoint and token; `BEE_ENDPOINT` and `BEE_TOKEN`
  override them, `--endpoint` overrides both.
- **Git:** `bee git setup` configures the remote `<endpoint>/<org>/<name>.git`, a credential
  helper that sends the token only to that host, and protocol v2 (git-gateway.md). Over SSH, use
  `ssh://git@<host>:2222/<org>/<name>.git` after adding a public key.
- **MCP:** the server speaks MCP (streamable HTTP) at `POST <endpoint>/mcp`, authenticated with
  `Authorization: Bearer <token>`. Point an MCP client at that URL.
- **Agent prompts:** the web app serves `/llms.txt` and `/prompts`, which describe the instance to
  agents.

## Verified

`scripts/check-selfhost.sh` runs this guide end to end against `deploy/compose.yml` as compose
project `selfhost` (server on host port 17420, web app on 13100) and tears it down with its
volumes. It fills a throwaway `deploy/.env` from `deploy/.env.example`, builds both images, adds a
pinned mock OIDC provider (`ghcr.io/navikt/mock-oauth2-server:2.1.10`) as the `[login]` issuer, and
then:

1. becomes the first admin as [First admin and SSO](#first-admin-and-sso) says (sign in, read the
   id from `WhoAmI`, put it in `site_admins`, recreate `bee-init` and `bee-server`);
2. creates an org and a public repo, lands a change with the `bee` CLI, clones over smart
   HTTP, pushes a second change with `git push` and checks it landed, and checks log, blame and
   diff, the web app's home, repo and admin pages, and `/llms.txt`;
3. takes a backup to the bucket and a restore drill, then deletes the Postgres and data volumes,
   starts a fresh stack, restores into cell `recovered` and points the server at it as
   [Backup and restore](#backup-and-restore) says, and compares every Bee ref, git ref and git
   object, the log, blame and file contents with the state before; fsck re-hashes every blob;
4. restarts the server and compares everything again, serves the stack through the caddy profile,
   and runs `--check-production`.

Two consecutive runs on 2026-11-15 (Apple M4 Pro, 14 cores, 24 GiB; Docker Desktop VM with 14
CPUs and 11.7 GiB; load average 8 to 10 from other builds; `BEE_CHECK_CARGO_BUILD_JOBS=4`),
both passing, both building the images from an empty build cache:

| Phase | Run 1 | Run 2 |
|---|---|---|
| Build images | 530 s | 883 s (about 274 s of it waiting for another build to finish) |
| Start the stack (`up --wait`) | 18 s | 18 s |
| First admin | 18 s | 17 s |
| Org, repo, land with the CLI | 3 s | 2 s |
| git clone and push | 1 s | 1 s |
| Log, blame, diff, web app | 1 s | 1 s |
| Backup to S3 and restore drill | 6 s | 6 s |
| Restore into a fresh stack | 57 s | 55 s |
| Restart survives | 14 s | 12 s |
| Caddy profile, production checklist | 3 s | 3 s |
| Teardown | 12 s | 11 s |
| **Total** | **663 s** | **1,009 s** |

Each restore loaded 104 metadata rows and 75 forge rows with no fsck violations, and the restored
instance served the same 17 git objects, git refs and Bee ref as before the disaster. The
images are 540 MB (`bee-server`) and 345 MB (`bee-web`); both run as a non-root user and
have a `HEALTHCHECK`. `.github/workflows/selfhost.yml` runs the script on every pull request.

## Troubleshooting

| Symptom | Check |
|---|---|
| `bee-init` exits non-zero | `docker compose -f deploy/compose.yml logs bee-init`. The message names the bad key. An unset `$VAR` is an error; fill it in `deploy/.env`. |
| `required variable ... is missing` from compose | A `CHANGE-ME` variable is not set in `deploy/.env`. |
| Server restarts, `postgres` errors | The password in `BEE_POSTGRES_URL` differs from `POSTGRES_PASSWORD`; the Postgres volume keeps the first password, so change it with `ALTER USER` or delete `postgres-data`. |
| Blob writes fail | `s3-init` did not create the bucket (`docker compose logs s3-init`), or the access and secret keys were changed after the `s3` volume was first initialised. |
| `/readyz` returns 503 | The metadata store is unreachable. Check Postgres health with `docker compose ps`. |
| Web app shows "not available on this server" | The server is older than the web app, so a call answered `UNIMPLEMENTED`. Upgrade both together. |
| Web app cannot reach the API | `BEE_API_URL` on `web` must be the address of `bee-server` on the compose network. |
| Git over SSH: host key changed | `/data` was recreated. Restore `ssh_host_ed25519_key` from backup or have users clear the old entry. |
| Tokens stop working after a restart | `BEE_SIGNING_KEY_HEX` changed. Restore the old value. |
| Image build is killed | Out of memory while compiling. Give Docker 8 GiB of memory, or lower the build's parallelism: the `Dockerfile` takes `--build-arg CARGO_BUILD_JOBS=1` (default 2; raise it on a bigger builder). |
