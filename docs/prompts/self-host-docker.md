---
id: self-host-docker
title: Self-host Beehive with Docker
when: An operator wants their own Beehive instance on one machine with Docker Compose. Covers the environment file, the first start, the production check and the first sign-in.
---
You are going to run a Beehive server and web app with Docker Compose and check that it is ready for real use. The authoritative guide is `docs/self-hosting.md` in the Bee repository; read it if anything below does not match what you see. A self-hosted instance runs the enterprise edition. You need Docker Engine 24 or newer with the Compose plugin (`docker compose version`), 2 CPUs and 4 GiB of memory (give Docker 8 GiB for the first build), and the shell of the machine that will host it.

1. Get the source and go to its root:

   ```sh
   git clone https://github.com/terramind-org/bee.git
   cd bee
   ```

2. Create the environment file and fill in every secret. Never commit `deploy/.env`:

   ```sh
   cp deploy/.env.example deploy/.env
   ```

   Replace each value marked `CHANGE-ME`. The token signing key `BEE_SIGNING_KEY_HEX` and the URL signing key `BEE_URL_KEY_HEX` are 64 hex digits each: generate each with `openssl rand -hex 32`. Choose one URL-safe Postgres password and use the same one in `POSTGRES_PASSWORD`, `BEE_POSTGRES_URL` and `BEE_FORGE_DATABASE`. Set `BEE_S3_ACCESS_KEY` (3 or more characters) and `BEE_S3_SECRET_KEY` (8 or more). Set `BEE_PUBLIC_URL` to the address people will use to open Beehive; for a trial on one machine the default `http://localhost:3100` is right. Losing the signing key signs everyone out and changing it invalidates every token, so keep a copy in the operator's secret store.

3. Review `deploy/server.toml`, the server configuration. Secrets stay in `deploy/.env`; the file refers to them as `$NAME`. To use the hosted Postgres or S3 you already have, change the matching URLs in `deploy/.env` and delete the `postgres` or `s3` services from `deploy/compose.yml`. `{{endpoint}}` is the URL this prompt was generated for; use your own address in `BEE_PUBLIC_URL`.

4. Build and start the stack:

   ```sh
   docker compose -f deploy/compose.yml build
   docker compose -f deploy/compose.yml up -d
   docker compose -f deploy/compose.yml ps
   ```

   `bee-init` validates the configuration first and stops the stack when it is wrong; read its output with `docker compose -f deploy/compose.yml logs bee-init`. Wait until `bee-server` and `web` are healthy. Check readiness:

   ```sh
   docker compose -f deploy/compose.yml exec bee-server curl -fsS http://127.0.0.1:7420/readyz
   ```

5. Run the production checklist in a throwaway server container. It prints each item and exits 1 when an error-level item fails; warnings do not block:

   ```sh
   docker compose -f deploy/compose.yml run --rm --no-deps bee-server --config /etc/bee/server.toml --check-production
   ```

   On a fresh install `operators` fails until you add the first site admin (step 6), and `tls` fails until `BEE_PUBLIC_URL` is an `https://` address (put Caddy in front: set `BEE_DOMAIN` and `BEE_PUBLIC_URL=https://<domain>` in `deploy/.env`, then `docker compose -f deploy/compose.yml --profile caddy up -d`). `metadata_replica` is a warning. Fix every other error item before you call the instance ready, and list the ones you could not fix.

6. Sign in and make the first administrator. Configure how people sign in in `deploy/server.toml` (`login.github_client_id` for the GitHub device flow, or `login.oidc_issuer` and `login.oidc_client_id` for OIDC), restart the server, and open the web app at `BEE_PUBLIC_URL`. Choose sign in. Read your principal id:

   ```sh
   curl -s -X POST "$BEE_PUBLIC_URL/bee.v1.AuthService/WhoAmI" \
     -H 'Content-Type: application/json' -H "Authorization: Bearer $BEE_TOKEN" -d '{}'
   ```

   Put it in `deploy/server.toml` as `[forge] site_admins = ["<your principal id>"]`, then restart with `docker compose -f deploy/compose.yml up -d`. Site admins are operators.

7. Sign in again and open `/admin` (for example `http://localhost:3100/admin`). The admin console shows server health, cluster status, configuration validation and enterprise settings (single sign-on, SCIM, IP allow lists, policies). Run the production check once more and confirm `operators` now passes.

Do not expose Postgres, S3 or the server port to the internet; the web app (or Caddy on 443) is the entry point. Back up the Postgres data, the S3 bucket and `/data` (it holds the key material); `docs/self-hosting.md` explains backups and restores. Report: which checklist items passed, which failed and why, the URL of the instance and whether `/admin` opened.
