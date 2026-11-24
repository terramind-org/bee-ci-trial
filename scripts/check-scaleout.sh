#!/usr/bin/env bash
# shellcheck disable=SC2016 # jq programs name their --arg variables in single quotes
# End-to-end check of the scaled-out stack (deploy/compose.scale.yml): three bee-server
# replicas on one Postgres 17 and one S3 store behind a Caddy load balancer, plus the web app,
# built from this checkout (docs/scaling.md).
#
#   1. writes a throwaway env file from deploy/.env.example plus the OpenID key a scaled-out
#      deployment needs (BEE_OIDC_KEY_HEX), a copy of
#      deploy/scale/server.toml with OIDC sign-in, a copy of deploy/scale/Caddyfile that names the
#      replica behind each answer (X-Bee-Upstream), and a compose override that adds a mock
#      OIDC provider and publishes every replica on its own loopback port;
#   2. builds both images and starts the stack as compose project `scaleout`;
#   3. becomes the first admin the way docs/self-hosting.md says, through the load balancer;
#   4. checks the balancer passes HTTP/JSON, gRPC (grpcurl) and git smart HTTP to all three
#      replicas, and that every replica publishes the same JWKS;
#   5. makes server-2 the land coalescer owner of main (SPEC 7.4), then lands
#      BEE_SCALE_LANDS (default 1600) files from BEE_SCALE_CLIENTS (default 32) concurrent
#      clients through the balancer, each its own file on main; `docker kill`s server-2 when 40%
#      are acknowledged and `docker start`s it BEE_SCALE_DEAD_SECS (default 10, longer than
#      the 6 s owner lease, so the lease expires and a survivor takes over) later, while lands
#      are still running;
#   6. asserts every acknowledged land is in history exactly once, history is linear, every file
#      is at the tip, each replica asked directly with min_version reports the same tip, a git
#      clone through the balancer has the same commit count, and the coalescer lease moved to a
#      surviving replica with a higher fence;
#   7. prints lands per second and p50/p99 time-to-land with the machine and load average, the
#      time each phase took, and tears the stack down with its volumes.
#
# BEE_CHECK_KEEP=1 leaves the stack running and the temp directory in place, for debugging.
# BEE_CHECK_SKIP_BUILD=1 uses the bee-server:latest and bee-web:latest images as
# they are. BEE_CHECK_CARGO_BUILD_JOBS sets the image build's rustc parallelism (default 2).
# BEE_CHECK_BUILD_LOCK=<file> runs the image build under `lockf <file>` (or `flock`).
# BEE_CHECK_RESULTS=<dir> keeps the load client's JSON results there.
# Needs docker (with compose), curl, git, jq, openssl, xxd and python3. Without docker it skips
# with exit 0, unless CI=true. Host ports, all on 127.0.0.1: 27420 (balancer), 23100 (web),
# 27421-27423 (replicas), 28080 (mock OIDC provider).
set -euo pipefail

cd "$(dirname "$0")/.."
NAME=check-scaleout
REPO_ROOT="$PWD"

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  if [ "${CI:-}" = "true" ]; then
    echo "$NAME: docker is required under CI=true but is not installed or its daemon is not running" >&2
    exit 1
  fi
  echo "$NAME: docker is not installed or its daemon is not running; skipping"
  exit 0
fi
for tool in curl git jq openssl xxd python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    if [ "${CI:-}" = "true" ]; then
      echo "$NAME: $tool is required under CI=true" >&2
      exit 1
    fi
    echo "$NAME: $tool is not installed; skipping"
    exit 0
  fi
done

PROJECT=scaleout
API_PORT=27420
WEB_PORT=23100
OIDC_PORT=28080
REPLICA_PORTS=(27421 27422 27423)
API="http://127.0.0.1:$API_PORT"
WEB="http://127.0.0.1:$WEB_PORT"
OIDC_HOST="mock-oidc:8080"
OIDC_ISSUER="http://$OIDC_HOST/default"
OIDC_CLIENT_ID="bee-scaleout"
GRPCURL_IMAGE="fullstorydev/grpcurl:v1.9.3"
LANDS="${BEE_SCALE_LANDS:-1600}"
DEAD_SECS="${BEE_SCALE_DEAD_SECS:-10}"
CLIENTS="${BEE_SCALE_CLIENTS:-32}"
TIMEOUT_SECS="${BEE_CHECK_TIMEOUT_SECS:-240}"
BUILD_JOBS="${BEE_CHECK_CARGO_BUILD_JOBS:-2}"
LOAD="$REPO_ROOT/deploy/scale/land-load.py"
START_TIME=$SECONDS

say() { echo "$NAME: $*"; }
die() { echo "$NAME: FAIL: $*" >&2; exit 1; }

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

[ "$LANDS" -ge 300 ] || die "BEE_SCALE_LANDS must be at least 300"
[ "$CLIENTS" -ge 32 ] || die "BEE_SCALE_CLIENTS must be at least 32"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/bee-scaleout.XXXXXX")"
ENV_FILE="$TMP/env"
OVERRIDE="$TMP/compose.override.yml"
SERVER_TOML="$TMP/server.toml"
CADDYFILE="$TMP/Caddyfile"
RESULTS="${BEE_CHECK_RESULTS:-$TMP/results}"
mkdir -p "$RESULTS"

COMPOSE=(docker compose -p "$PROJECT" --env-file "$ENV_FILE" -f deploy/compose.scale.yml -f "$OVERRIDE")

TIMINGS=()
PHASE=""
PHASE_T0=$SECONDS
phase() {
  if [ -n "$PHASE" ]; then
    TIMINGS+=("$(printf '%-34s %5ss' "$PHASE" "$((SECONDS - PHASE_T0))")")
  fi
  PHASE="$1"
  PHASE_T0=$SECONDS
  [ -z "$PHASE" ] || say "== $PHASE"
}
print_timings() {
  [ "${#TIMINGS[@]}" -gt 0 ] || return 0
  echo "$NAME: timings"
  printf '  %s\n' "${TIMINGS[@]}"
  printf '  %-34s %5ss\n' "total" "$((SECONDS - START_TIME))"
}

cleanup() {
  status=$?
  trap - EXIT
  if [ "$status" -ne 0 ] && [ -n "$PHASE" ]; then
    TIMINGS+=("$(printf '%-34s %5ss (failed)' "$PHASE" "$((SECONDS - PHASE_T0))")")
  fi
  if [ -n "${BEE_CHECK_KEEP:-}" ]; then
    print_timings
    echo "$NAME: BEE_CHECK_KEEP is set; leaving project $PROJECT running (state in $TMP)"
    exit "$status"
  fi
  if [ "$status" -ne 0 ] && [ -f "$ENV_FILE" ] && [ -f "$OVERRIDE" ]; then
    echo "$NAME: failed; container logs follow" >&2
    "${COMPOSE[@]}" ps -a >&2 || true
    "${COMPOSE[@]}" logs --no-color --tail 60 >&2 || true
  fi
  if [ -f "$ENV_FILE" ] && [ -f "$OVERRIDE" ]; then
    "${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP" 2>/dev/null || true
  [ "$status" -eq 0 ] || print_timings
  exit "$status"
}
trap cleanup EXIT

machine() {
  local cpu mem
  if [ "$(uname -s)" = Darwin ]; then
    cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sysctl -n hw.ncpu) cores"
    mem="$(($(sysctl -n hw.memsize) / 1073741824)) GiB"
  else
    cpu="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //'), $(nproc) cores"
    mem="$(awk '/MemTotal/ { printf "%d GiB", $2 / 1048576 }' /proc/meminfo)"
  fi
  echo "$(uname -s) $(uname -m), $cpu, $mem; docker $(docker version --format '{{.Server.Version}}' 2>/dev/null) with $(docker info --format '{{.NCPU}} CPUs, {{.MemTotal}}' 2>/dev/null | awk '{ printf "%s %s %.1f GiB", $1, $2, $3 / 1073741824 }')"
}
loadavg() { uptime | sed 's/.*load average[s]*: //'; }

# ---------------------------------------------------------------------------------------------
phase "configure"
# ---------------------------------------------------------------------------------------------
rand() { openssl rand -hex "$1"; }
PG_PASSWORD="$(rand 16)"
S3_ACCESS_KEY="ak$(rand 8)"
S3_SECRET_KEY="sk$(rand 16)"
SIGNING_KEY="$(rand 32)"
URL_KEY="$(rand 32)"
sed \
  -e "s|CHANGE-ME-postgres-password|$PG_PASSWORD|g" \
  -e "s|CHANGE-ME-s3-access-key|$S3_ACCESS_KEY|" \
  -e "s|CHANGE-ME-s3-secret-key|$S3_SECRET_KEY|" \
  -e "s|CHANGE-ME-64-hex-digits|$SIGNING_KEY|" \
  -e "s|CHANGE-ME-url-key-hex-digits|$URL_KEY|" \
  -e "s|^BEE_PUBLIC_URL=.*|BEE_PUBLIC_URL=$API|" \
  -e "s|^BEE_API_PORT=.*|BEE_API_PORT=$API_PORT|" \
  -e "s|^BEE_WEB_PORT=.*|BEE_WEB_PORT=$WEB_PORT|" \
  deploy/.env.example > "$ENV_FILE"
if grep -v '^#' "$ENV_FILE" | grep -q 'CHANGE-ME'; then
  grep -n '^[^#]*CHANGE-ME' "$ENV_FILE" >&2
  die "deploy/.env.example has a CHANGE-ME this script does not fill in"
fi
# The OpenID key deploy/compose.scale.yml asks for, generated the way its header says. The URL
# signing key is already in the env file: deploy/.env.example carries it for every stack.
OIDC_KEY_HEX="$(openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 2>/dev/null | openssl pkcs8 -topk8 -nocrypt -outform DER 2>/dev/null | xxd -p | tr -d '\n')"
[ -n "$OIDC_KEY_HEX" ] || die "openssl could not generate a P-256 key"
echo "BEE_OIDC_KEY_HEX=$OIDC_KEY_HEX" >> "$ENV_FILE"

cp deploy/scale/server.toml "$SERVER_TOML"
grep -q '^# oidc_issuer = ' "$SERVER_TOML" || die "deploy/scale/server.toml lost its [login] example"
cat >> "$SERVER_TOML" <<EOF

[login]
oidc_issuer = "$OIDC_ISSUER"
oidc_client_id = "$OIDC_CLIENT_ID"
EOF

# The balancer as shipped, plus a response header naming the replica that answered.
sed 's|^\([[:space:]]*\)lb_policy round_robin$|&\n\1header_down X-Bee-Upstream {http.reverse_proxy.upstream.hostport}|' \
  deploy/scale/Caddyfile > "$CADDYFILE"
grep -q 'header_down X-Bee-Upstream' "$CADDYFILE" || die "deploy/scale/Caddyfile has no 'lb_policy round_robin' line"

{
  echo "# Test-only additions to deploy/compose.scale.yml."
  echo "services:"
  echo "  mock-oidc:"
  echo "    image: ghcr.io/navikt/mock-oauth2-server:2.1.10"
  echo "    restart: unless-stopped"
  echo "    ports:"
  echo "      - \"127.0.0.1:$OIDC_PORT:8080\""
  echo "  bee-init:"
  echo "    volumes:"
  echo "      - $SERVER_TOML:/etc/bee/server.toml:ro"
  for n in 1 2 3; do
    echo "  server-$n:"
    echo "    volumes:"
    echo "      - $SERVER_TOML:/etc/bee/server.toml:ro"
    echo "    ports:"
    echo "      - \"127.0.0.1:${REPLICA_PORTS[$((n - 1))]}:7420\""
  done
  echo "  lb:"
  echo "    volumes:"
  echo "      - $CADDYFILE:/etc/caddy/Caddyfile:ro"
} > "$OVERRIDE"
SERVER_IMAGE="$(grep -E '^BEE_SERVER_IMAGE=' "$ENV_FILE" | cut -d= -f2)"
"${COMPOSE[@]}" config -q
# A stack left over from an interrupted run would collide with this one. (compose needs the env
# file to read deploy/compose.scale.yml at all, so this comes after it is written.)
"${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
for p in $API_PORT $WEB_PORT $OIDC_PORT "${REPLICA_PORTS[@]}"; do
  ! port_busy "$p" || die "host port $p is already in use; stop whatever listens on it"
done
say "machine: $(machine)"
say "load average at start: $(loadavg)"

# ---------------------------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------------------------
wait_for_code() { # name url expected-code
  local name=$1 url=$2 want=$3 waited=0
  until [ "$(curl -sL -o /dev/null -w '%{http_code}' "$url" || true)" = "$want" ]; do
    [ "$waited" -lt "$TIMEOUT_SECS" ] || die "$name did not answer $want at $url within ${TIMEOUT_SECS}s"
    sleep 1
    waited=$((waited + 1))
  done
}
upstream_of() { # the replica the balancer sent one /readyz to
  curl -s -o /dev/null -D - "$API/readyz" | awk 'tolower($1) == "x-bee-upstream:" { print $2 }' | tr -d '\r'
}
wait_replicas() { # every replica ready, and the balancer routing to all three again
  local n waited=0 seen=""
  for n in 1 2 3; do
    wait_for_code "server-$n /readyz" "http://127.0.0.1:${REPLICA_PORTS[$((n - 1))]}/readyz" 200
  done
  wait_for_code "balancer /readyz" "$API/readyz" 200
  # The balancer's health checks put a replica that restarted back within a few seconds.
  until [ "$(tr ' ' '\n' <<<"$seen" | grep -c 'server-')" -ge 3 ]; do
    [ "$waited" -lt 300 ] || die "the balancer did not route to all three replicas within 30s (saw:$seen)"
    u="$(upstream_of)"
    grep -qw -- "$u" <<<"$seen" || seen="$seen $u"
    sleep 0.1
    waited=$((waited + 1))
  done
}

# rpc <Service/Method> <json> [token] [base]: POST the proto3-JSON request, print the response,
# fail on non-200.
rpc() {
  local method=$1 body=$2 token=${3-${TOKEN:-}} base=${4:-$API} out code
  out="$TMP/rpc.out"
  local auth=()
  [ -z "$token" ] || auth=(-H "Authorization: Bearer $token")
  code="$(curl -sS -o "$out" -w '%{http_code}' -X POST "$base/bee.v1.$method" \
    -H 'Content-Type: application/json' ${auth[@]+"${auth[@]}"} -d "$body")" \
    || die "$method: the request failed"
  [ "$code" = "200" ] || die "$method answered HTTP $code: $(cat "$out")"
  cat "$out"
}

assert() {
  local what=$1
  shift
  "$@" >/dev/null 2>&1 || die "assertion failed: $what"
  say "ok: $what"
}

oidc_id_token() {
  local q="client_id=$OIDC_CLIENT_ID&response_type=code&redirect_uri=http://localhost/callback&scope=openid&state=s"
  local base="http://127.0.0.1:$OIDC_PORT/default" location code
  location="$(curl -sS -i -H "Host: $OIDC_HOST" -X POST -d 'username=admin&claims=' "$base/authorize?$q" \
    | awk 'tolower($1) == "location:" { print $2 }' | tr -d '\r')"
  code="${location#*code=}"
  code="${code%%&*}"
  if [ -z "$code" ] || [ "$code" = "$location" ]; then
    die "the mock OIDC provider issued no code"
  fi
  curl -sS -H "Host: $OIDC_HOST" -X POST \
    -d "grant_type=authorization_code&code=$code&client_id=$OIDC_CLIENT_ID&client_secret=unused&redirect_uri=http://localhost/callback" \
    "$base/token" | jq -er .id_token
}
oidc_login() {
  local jwt
  jwt="$(oidc_id_token)"
  rpc AuthService/Login "$(jq -nc --arg t "$jwt" '{oidcIdToken: $t}')" "" | jq -er .token
}

GITHOME="$TMP/githome"
mkdir -p "$GITHOME"
g() {
  HOME="$GITHOME" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
    git -c "http.extraHeader=Authorization: Bearer $TOKEN" "$@"
}
grpc() { # <Service/Method> <json>: a gRPC call through the balancer, inside the compose network
  docker run --rm --network "${PROJECT}_default" -v "$REPO_ROOT/proto:/proto:ro" "$GRPCURL_IMAGE" \
    -plaintext -max-time 30 -import-path /proto -proto bee/v1/vcs.proto -proto bee/v1/auth.proto \
    -H "authorization: Bearer $TOKEN" -d "$2" lb:7420 "bee.v1.$1"
}
lease_main() { # the coalescer lease row of acme/app main, as JSON
  "${COMPOSE[@]}" exec -T postgres psql -U bee -d bee -Atc \
    "select convert_from(v, 'UTF8') from bee.bee_server_kv where ns = 'lease' and convert_from(k, 'UTF8') = '$REPO_ID/main'"
}
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }
metrics_line() { # replica-port: the land metrics of one replica
  curl -s "http://127.0.0.1:$1/metrics" \
    | grep -E '^bee_(lands_total|land_batches_total|coalescer_owner_changes_total)' | tr '\n' ' '
}

# ---------------------------------------------------------------------------------------------
phase "build images"
# ---------------------------------------------------------------------------------------------
locked() {
  if [ -z "${BEE_CHECK_BUILD_LOCK:-}" ]; then
    "$@"
  elif command -v lockf >/dev/null 2>&1; then
    lockf "$BEE_CHECK_BUILD_LOCK" "$@"
  else
    flock "$BEE_CHECK_BUILD_LOCK" "$@"
  fi
}
if [ -n "${BEE_CHECK_SKIP_BUILD:-}" ]; then
  say "BEE_CHECK_SKIP_BUILD is set; using the images as they are"
else
  locked "${COMPOSE[@]}" build --build-arg "CARGO_BUILD_JOBS=$BUILD_JOBS" \
    --build-arg "BUILD_SHA=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
fi
docker image inspect "$SERVER_IMAGE" bee-web:latest >/dev/null || die "the images are missing"
docker pull -q "$GRPCURL_IMAGE" >/dev/null

# ---------------------------------------------------------------------------------------------
phase "start stack"
# ---------------------------------------------------------------------------------------------
# All three replicas start at once on an empty database: their schema migrations race.
"${COMPOSE[@]}" up -d --wait --wait-timeout "$TIMEOUT_SECS"
"${COMPOSE[@]}" ps --format '{{.Service}} {{.Status}}' | sed "s/^/$NAME:   /"
wait_replicas
wait_for_code "web /" "$WEB/" 200
for n in 1 2 3; do
  restarts="$(docker inspect --format '{{.RestartCount}}' "$("${COMPOSE[@]}" ps -q "server-$n")")"
  assert "server-$n started on the first attempt (restart count $restarts)" test "$restarts" = 0
done

# ---------------------------------------------------------------------------------------------
phase "first admin"
# ---------------------------------------------------------------------------------------------
TOKEN="$(oidc_login)"
ADMIN_ID="$(rpc AuthService/WhoAmI '{}' | jq -er .principal.id)"
say "first sign-in made principal $ADMIN_ID"
sed "s|^site_admins = \[\]|site_admins = [\"$ADMIN_ID\"]|" "$SERVER_TOML" > "$TMP/server.toml.new"
grep -q "site_admins = \[\"$ADMIN_ID\"\]" "$TMP/server.toml.new" || die "deploy/scale/server.toml has no 'site_admins = []' line to edit"
cat "$TMP/server.toml.new" > "$SERVER_TOML"
"${COMPOSE[@]}" up -d --wait --wait-timeout "$TIMEOUT_SECS" --force-recreate --no-deps bee-init server-1 server-2 server-3
wait_replicas
for n in 1 2 3; do
  assert "the token issued before the recreate verifies on server-$n directly" \
    test "$(rpc AuthService/WhoAmI '{}' "$TOKEN" "http://127.0.0.1:${REPLICA_PORTS[$((n - 1))]}" | jq -er .principal.id)" = "$ADMIN_ID"
done
assert "the admin is an operator now (ListBackups answers)" rpc AdminService/ListBackups '{}'

# ---------------------------------------------------------------------------------------------
phase "rotate signing keys across replicas"
# ---------------------------------------------------------------------------------------------
# The token signing key in use, from the server's own list. (/.well-known/jwks.json publishes the
# OpenID key, whose kid is not a token signing key id.)
OLD_SIGNING_KEY="$(rpc AuthService/ListSigningKeys '{}' | jq -er '.keys[] | select(.signing == true) | .keyId')"
NEW_SIGNING_KEY="$(rpc AdminService/RotateSigningKey '{"algorithm":"ed25519"}' | jq -er .key.keyId)"
# A replica starts signing with the new key at its next refresh of the shared key ring
# (auth.revocation_refresh_ms, one second by default). Wait until all three do, so the login
# below cannot be signed with the key this phase is about to retire.
signing_key_of() { # replica number: the id of the key that replica signs new tokens with
  rpc AuthService/ListSigningKeys '{}' "$TOKEN" "http://127.0.0.1:${REPLICA_PORTS[$(($1 - 1))]}" \
    | jq -r '.keys[] | select(.signing == true) | .keyId'
}
for n in 1 2 3; do
  for _ in $(seq 1 20); do
    [ "$(signing_key_of "$n")" = "$NEW_SIGNING_KEY" ] && break
    sleep 0.5
  done
  assert "server-$n signs new tokens with key $NEW_SIGNING_KEY after the rotation" \
    test "$(signing_key_of "$n")" = "$NEW_SIGNING_KEY"
done
NEW_TOKEN="$(oidc_login)"
assert "a token minted after rotation on server-1 verifies on server-2" \
  test "$(rpc AuthService/WhoAmI '{}' "$NEW_TOKEN" "http://127.0.0.1:${REPLICA_PORTS[1]}" | jq -er .principal.id)" = "$ADMIN_ID"
rpc AdminService/RetireSigningKey "$(jq -nc --arg id "$OLD_SIGNING_KEY" '{keyId: $id}')" >/dev/null
# A replica learns of a retirement at its next refresh of the shared key ring
# (auth.revocation_refresh_ms, one second by default), so give server-3 a few intervals.
OLD_STATUS=200
for _ in $(seq 1 20); do
  OLD_STATUS="$(curl -sS -o "$TMP/retired-token.out" -w '%{http_code}' -X POST "http://127.0.0.1:${REPLICA_PORTS[2]}/bee.v1.AuthService/WhoAmI" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d '{}')"
  [ "$OLD_STATUS" != 200 ] && break
  sleep 0.5
done
assert "server-3 refuses a token signed by retired key $OLD_SIGNING_KEY (new key $NEW_SIGNING_KEY)" test "$OLD_STATUS" != 200
# The admin token used so far was signed by the key just retired; carry on with the new one.
TOKEN="$NEW_TOKEN"

# ---------------------------------------------------------------------------------------------
phase "balancer passes every protocol"
# ---------------------------------------------------------------------------------------------
rpc OrgService/CreateOrg '{"name":"acme","display":"Acme"}' >/dev/null
REPO="$(rpc RepoService/CreateRepo '{"org":"acme","name":"app","visibility":"VISIBILITY_PUBLIC","description":"Scale-out check","autoInit":true}')"
REPO_ID="$(jq -er .id <<<"$REPO")"
UPSTREAMS="$(for _ in 1 2 3 4 5 6 7 8 9; do
  curl -sS -o /dev/null -D - -X POST "$API/bee.v1.AuthService/WhoAmI" -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $TOKEN" -d '{}' | awk 'tolower($1) == "x-bee-upstream:" { print $2 }' | tr -d '\r'
done | sort | uniq -c | awk '{ printf "%s=%s ", $2, $1 }')"
say "nine HTTP/JSON calls were answered by: $UPSTREAMS"
assert "HTTP/JSON reaches all three replicas round robin" \
  test "$(wc -w <<<"$UPSTREAMS" | tr -d ' ')" = 3
GRPC_REF="$(grpc ObjectService/ResolveRef '{"repo":{"fullName":"acme/app"},"name":"main"}')"
assert "gRPC through the balancer resolves main (version $(jq -r .ref.version <<<"$GRPC_REF"))" \
  test "$(jq -r .ref.version <<<"$GRPC_REF")" -ge 1
GRPC_HEAD="$(BEE_TOKEN="$TOKEN" python3 "$LOAD" prepare --endpoint "$API" --repo acme/app --count 1 --prefix grpc | jq -er .source)"
GRPC_LAND="$(grpc WriteService/Land "$(jq -nc --arg h "$GRPC_HEAD" '{repo: {fullName: "acme/app"}, ref: "main", head: {raw: $h}}')")"
assert "a Land over gRPC through the balancer lands (version $(jq -r .ref.version <<<"$GRPC_LAND"))" \
  test "$(jq -r '.landed | length' <<<"$GRPC_LAND")" = 1 -a "$(rpc ObjectService/ResolveRef '{"repo":{"fullName":"acme/app"},"name":"main"}' | jq -r .ref.target.raw)" = "$(jq -r .ref.target.raw <<<"$GRPC_LAND")"
g clone -q "$API/acme/app.git" "$TMP/gitwc"
assert "git clone through the balancer has README.md and the gRPC-landed file" test -f "$TMP/gitwc/README.md" -a -f "$TMP/gitwc/grpc/0000.txt"
echo "pushed through the balancer" > "$TMP/gitwc/from-git.txt"
g -C "$TMP/gitwc" add from-git.txt
g -C "$TMP/gitwc" -c user.name="Scale Out" -c user.email=scaleout@example.com commit -q -m "Add from-git.txt"
g -C "$TMP/gitwc" push -q origin HEAD:main
assert "git push through the balancer landed on main" \
  test "$(g ls-remote "$API/acme/app.git" refs/heads/main | cut -f1)" = "$(g -C "$TMP/gitwc" rev-parse HEAD)"
JWKS_LB="$(curl -sf "$API/.well-known/jwks.json")"
for n in 1 2 3; do
  assert "server-$n publishes the same JWKS as the balancer (kid $(jq -r '.keys[0].kid' <<<"$JWKS_LB"))" \
    test "$(curl -sf "http://127.0.0.1:${REPLICA_PORTS[$((n - 1))]}/.well-known/jwks.json")" = "$JWKS_LB"
done
assert "the web app, which reaches the API through the balancer, serves the repo page" \
  test "$(curl -sL -o "$TMP/page.html" -w '%{http_code}' "$WEB/acme/app")" = 200 -a "$(grep -c 'acme/app' "$TMP/page.html")" -ge 1

# ---------------------------------------------------------------------------------------------
phase "make server-2 the coalescer owner"
# ---------------------------------------------------------------------------------------------
# The owner of main's land coalescer keeps its lease while it has work and gives it up after
# 30 s idle. Wait until no replica holds it, then land through server-2 directly so server-2
# takes it: killing server-2 below then kills the owner, the hardest case for takeover.
waited=0
while row="$(lease_main)" && [ -n "$row" ] && [ "$(jq -r .expires_ms <<<"$row")" -gt "$(now_ms)" ]; do
  [ "$waited" -lt 90 ] || die "the lease of main is still held after 90s: $row"
  sleep 1
  waited=$((waited + 1))
done
say "no replica holds main's lease (waited ${waited}s)"
BEE_TOKEN="$TOKEN" python3 "$LOAD" run --endpoint "http://127.0.0.1:${REPLICA_PORTS[1]}" --repo acme/app \
  --clients 2 --lands 4 --out "$RESULTS/warmup.json" >/dev/null
LEASE_BEFORE="$(lease_main)"
say "lease before the load: $(jq -c '{endpoint, fence}' <<<"$LEASE_BEFORE")"
assert "server-2 owns main's coalescer" test "$(jq -r .endpoint <<<"$LEASE_BEFORE")" = "http://server-2:7420"

# ---------------------------------------------------------------------------------------------
phase "load with server-2 killed"
# ---------------------------------------------------------------------------------------------
SERVER2="$("${COMPOSE[@]}" ps -q server-2)"
say "load average before the load: $(loadavg)"
LOAD_AVG_BEFORE="$(loadavg)"
BEE_TOKEN="$TOKEN" python3 "$LOAD" run --endpoint "$API" --repo acme/app \
  --clients "$CLIENTS" --lands "$LANDS" --out "$RESULTS/run.json" \
  --hook "0.4=docker kill $SERVER2" --hook "0.4=sleep $DEAD_SECS && docker start $SERVER2" \
  || die "the load run failed (results in $RESULTS/run.json)"
LOAD_AVG_AFTER="$(loadavg)"
RUN="$RESULTS/run.json"
jq -r '.hooks[] | "hook: \(.cmd | sub(" [0-9a-f]{64}$"; " server-2")) at \(.at_acked) acknowledged lands, \(.started_s)s into the \(.wall)s run, done \(.started_s + .took_s | . * 1000 | round / 1000)s in"' \
  <<<"$(jq '.wall_s as $w | .hooks |= map(. + {wall: $w})' "$RUN")" | sed "s/^/$NAME:   /"
assert "every one of $LANDS lands from $CLIENTS clients was acknowledged" \
  jq -e --argjson n "$LANDS" '.acked == $n and .failed == 0' "$RUN"
assert "server-2 was killed while lands were still running" \
  jq -e --argjson n "$LANDS" '[.hooks[] | select(.cmd | startswith("docker kill"))] | length == 1 and .[0].exit == 0 and .[0].at_acked < $n' "$RUN"
assert "server-2 was started again ${DEAD_SECS}s later, while lands were still running" \
  jq -e '.wall_s as $w | [.hooks[] | select(.cmd | contains("docker start"))] | length == 1 and .[0].exit == 0 and (.[0].started_s + .[0].took_s) < $w' "$RUN"
wait_replicas
say "server-2 is ready again after its restart"

# ---------------------------------------------------------------------------------------------
phase "verify"
# ---------------------------------------------------------------------------------------------
BEE_TOKEN="$TOKEN" python3 "$LOAD" verify --endpoint "$API" --repo acme/app --results "$RUN" \
  --replica "http://127.0.0.1:${REPLICA_PORTS[0]}" --replica "http://127.0.0.1:${REPLICA_PORTS[1]}" \
  --replica "http://127.0.0.1:${REPLICA_PORTS[2]}" --out "$RESULTS/verify.json" \
  || die "verification failed (results in $RESULTS)"
VERIFY="$RESULTS/verify.json"
say "ok: every acknowledged land is in history exactly once, at the changeset Land answered"
say "ok: history is linear ($(jq -r .log_count "$VERIFY") changesets) and every file is at the tip ($(jq -r .files_at_tip "$VERIFY"))"
say "ok: server-1, server-2 and server-3 asked directly with min_version $(jq -r .tip_version "$VERIFY") report the same tip"
rm -rf "$TMP/clone"
g clone -q "$API/acme/app.git" "$TMP/clone"
GIT_COUNT="$(g -C "$TMP/clone" rev-list --count HEAD)"
LOG_COUNT="$(jq -r .log_count "$VERIFY")"
assert "a git clone through the balancer has the same commit count as history ($GIT_COUNT)" test "$GIT_COUNT" = "$LOG_COUNT"
assert "the clone has every landed file" \
  test "$(find "$TMP/clone/scale/$(jq -r .run "$RUN")" -type f | wc -l | tr -d ' ')" = "$LANDS"
LEASE_AFTER="$(lease_main)"
say "lease after the load: $(jq -c '{endpoint, fence}' <<<"$LEASE_AFTER")"
assert "a surviving replica took main's coalescer over with a higher fence" \
  test "$(jq -r .endpoint <<<"$LEASE_AFTER")" != "http://server-2:7420" -a \
  "$(jq -r .fence <<<"$LEASE_AFTER")" -gt "$(jq -r .fence <<<"$LEASE_BEFORE")"
for n in 1 2 3; do
  say "server-$n metrics: $(metrics_line "${REPLICA_PORTS[$((n - 1))]}")"
done
# Postgres connections each replica holds after the load (docs/scaling.md sizes the database by
# these).
for n in 1 2 3; do
  ip="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$("${COMPOSE[@]}" ps -q "server-$n")")"
  say "server-$n holds $("${COMPOSE[@]}" exec -T postgres psql -U bee -d bee -Atc \
    "select count(*) from pg_stat_activity where client_addr = '$ip'") Postgres connections"
done

# ---------------------------------------------------------------------------------------------
phase "teardown"
# ---------------------------------------------------------------------------------------------
"${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1
phase ""
jq -r --arg m "$(machine)" --arg before "$LOAD_AVG_BEFORE" --arg after "$LOAD_AVG_AFTER" '
  "throughput: \(.acked) lands from \(.clients) clients in \(.wall_s) s = \(.lands_per_s) lands/s",
  "time to land (Land RPC): p50 \(.land_ms.p50) ms, p99 \(.land_ms.p99) ms, max \(.land_ms.max) ms",
  "upload + create + land per file: p50 \(.e2e_ms.p50) ms, p99 \(.e2e_ms.p99) ms, max \(.e2e_ms.max) ms",
  "Land answered by: \(.land_upstreams | to_entries | map("\(.key)=\(.value)") | join(" "))",
  "retried requests: \(.retries | to_entries | map("\(.key)=\(.value)") | join(", ") | if . == "" then "none" else . end)",
  "lands answered skipped on retry (already landed): \(.skipped_on_retry)",
  "machine: \($m)",
  "load average before / after the load: \($before) / \($after)"' "$RUN" | sed "s/^/$NAME: /"
print_timings
echo "$NAME: ok"
