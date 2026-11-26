#!/usr/bin/env bash
# shellcheck disable=SC2016 # jq programs name their --arg variables in single quotes
# End-to-end check of the production Docker Compose stack (deploy/compose.yml): Postgres 17, an
# S3 store, bee-server and the web app, built from this checkout and driven the way an
# operator and an agent drive them (docs/self-hosting.md).
#
#   1. writes a throwaway env file from deploy/.env.example (random secrets), a copy of
#      deploy/server.toml with OIDC sign-in, and a compose override that adds a mock OIDC
#      provider and publishes the server on 17420 and the web app on 13100, in a temp directory;
#   2. builds both images and starts the stack as compose project `selfhost`;
#   3. becomes the first admin as docs/self-hosting.md ("First admin and SSO") says: sign in
#      through the OIDC provider, read the principal id from AuthService.WhoAmI, put it in
#      forge.site_admins, recreate the server;
#   4. creates an org and a repo, lands a change with the bee CLI, clones and pushes with git
#      over smart HTTP, reads log, blame and diff, fetches the web app's pages;
#   5. backs up to S3, runs a restore drill, then destroys the Postgres and data volumes (the
#      bucket survives, as it would on real object storage), starts a fresh stack, restores the
#      backup into it, points the server at the restored data and checks every ref and object;
#   6. restarts the server and checks everything survived; checks the caddy profile; runs
#      `bee-server --check-production`;
#   7. prints the time each phase took and tears the stack down with its volumes (container logs
#      first when anything failed).
#
# BEE_CHECK_KEEP=1 leaves the stack running and the temp directory in place, for debugging.
# BEE_CHECK_CARGO_BUILD_JOBS sets the image build's rustc parallelism (default 2).
# BEE_CHECK_BUILD_LOCK=<file> runs the image build under `lockf <file>` (or `flock` on Linux),
# for machines where heavy builds share one lock.
# Needs docker (with compose and buildkit), curl, git, jq and openssl. Without docker it skips
# with exit 0, unless CI=true. Host ports, all on 127.0.0.1: 17420 (server), 13100 (web),
# 12222 (ssh), 18080 (mock OIDC provider), 18081 and 18443 (caddy).
set -euo pipefail

cd "$(dirname "$0")/.."
NAME=check-selfhost

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  if [ "${CI:-}" = "true" ]; then
    echo "$NAME: docker is required under CI=true but is not installed or its daemon is not running" >&2
    exit 1
  fi
  echo "$NAME: docker is not installed or its daemon is not running; skipping"
  exit 0
fi
for tool in curl git jq openssl; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    if [ "${CI:-}" = "true" ]; then
      echo "$NAME: $tool is required under CI=true" >&2
      exit 1
    fi
    echo "$NAME: $tool is not installed; skipping"
    exit 0
  fi
done

PROJECT=selfhost
API_PORT=17420
WEB_PORT=13100
SSH_PORT=12222
OIDC_PORT=18080
HTTP_PORT=18081
HTTPS_PORT=18443
API="http://127.0.0.1:$API_PORT"
WEB="http://127.0.0.1:$WEB_PORT"
OIDC_HOST="mock-oidc:8080"
OIDC_ISSUER="http://$OIDC_HOST/default"
OIDC_CLIENT_ID="bee-selfhost"
TIMEOUT_SECS="${BEE_CHECK_TIMEOUT_SECS:-240}"
BUILD_JOBS="${BEE_CHECK_CARGO_BUILD_JOBS:-2}"
START_TIME=$SECONDS

say() { echo "$NAME: $*"; }
die() { echo "$NAME: FAIL: $*" >&2; exit 1; }

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/bee-selfhost.XXXXXX")"
ENV_FILE="$TMP/env"
OVERRIDE="$TMP/compose.override.yml"
SERVER_TOML="$TMP/server.toml"
WORK="$TMP/work"
mkdir -p "$WORK"
chmod 777 "$WORK" # the CLI container runs as the invoking user, whatever its uid

COMPOSE=(docker compose -p "$PROJECT" --env-file "$ENV_FILE" -f deploy/compose.yml -f "$OVERRIDE")

# Phase timings, printed at the end (and on failure).
TIMINGS=()
PHASE=""
PHASE_T0=$SECONDS
phase() { # starts a phase, closing the previous one
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
    "${COMPOSE[@]}" --profile caddy ps -a >&2 || true
    "${COMPOSE[@]}" --profile caddy logs --no-color --tail 80 >&2 || true
  fi
  if [ -z "${TORN_DOWN:-}" ] && [ -f "$ENV_FILE" ] && [ -f "$OVERRIDE" ]; then
    "${COMPOSE[@]}" --profile caddy down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP" 2>/dev/null || true
  [ "$status" -eq 0 ] || print_timings
  exit "$status"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------------------------
phase "configure"
# ---------------------------------------------------------------------------------------------
# A stack left over from an interrupted run would collide with this one.
docker compose -p "$PROJECT" -f deploy/compose.yml --profile caddy down --volumes --remove-orphans >/dev/null 2>&1 || true
for p in $API_PORT $WEB_PORT $SSH_PORT $OIDC_PORT $HTTP_PORT $HTTPS_PORT; do
  ! port_busy "$p" || die "host port $p is already in use; stop whatever listens on it"
done

# Every CHANGE-ME in deploy/.env.example is replaced; the script refuses to run if the example
# gains a placeholder it does not know.
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
  -e "s|^BEE_SSH_PORT=.*|BEE_SSH_PORT=$SSH_PORT|" \
  -e "s|^BEE_API_PORT=.*|BEE_API_PORT=$API_PORT|" \
  -e "s|^BEE_WEB_PORT=.*|BEE_WEB_PORT=$WEB_PORT|" \
  -e "s|^BEE_HTTP_PORT=.*|BEE_HTTP_PORT=$HTTP_PORT|" \
  -e "s|^BEE_HTTPS_PORT=.*|BEE_HTTPS_PORT=$HTTPS_PORT|" \
  deploy/.env.example > "$ENV_FILE"
if grep -v '^#' "$ENV_FILE" | grep -q 'CHANGE-ME'; then
  grep -n '^[^#]*CHANGE-ME' "$ENV_FILE" >&2
  die "deploy/.env.example has a CHANGE-ME this script does not fill in"
fi
for key in BEE_PUBLIC_URL BEE_API_PORT BEE_WEB_PORT BEE_SSH_PORT BEE_HTTP_PORT BEE_HTTPS_PORT; do
  grep -q "^$key=" "$ENV_FILE" || die "deploy/.env.example does not set $key"
done

# The server configuration the operator edits, copied so the checkout stays untouched: sign-in
# through the mock OIDC provider, as the commented [login] block of deploy/server.toml says.
# site_admins stays empty until the first admin has signed in.
cp deploy/server.toml "$SERVER_TOML"
grep -q '^# oidc_issuer = ' "$SERVER_TOML" || die "deploy/server.toml lost its [login] example"
cat >> "$SERVER_TOML" <<EOF

[login]
oidc_issuer = "$OIDC_ISSUER"
oidc_client_id = "$OIDC_CLIENT_ID"
EOF

cat > "$OVERRIDE" <<EOF
# Test-only additions to deploy/compose.yml: a pinned mock OIDC provider and the edited
# server.toml. Ports come from the env file.
services:
  mock-oidc:
    image: ghcr.io/navikt/mock-oauth2-server:2.1.10
    restart: unless-stopped
    ports:
      - "127.0.0.1:$OIDC_PORT:8080"
  bee-init:
    volumes:
      - $SERVER_TOML:/etc/bee/server.toml:ro
  bee-server:
    volumes:
      - $SERVER_TOML:/etc/bee/server.toml:ro
EOF
SERVER_IMAGE="$(grep -E '^BEE_SERVER_IMAGE=' "$ENV_FILE" | cut -d= -f2)"
"${COMPOSE[@]}" config -q
"${COMPOSE[@]}" --profile caddy config -q

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

# rpc <Service/Method> <json> [token]: POST the proto3-JSON request, print the response, fail
# on non-200.
rpc() {
  local method=$1 body=$2 token=${3-${TOKEN:-}} out code
  out="$TMP/rpc.out"
  local auth=()
  [ -z "$token" ] || auth=(-H "Authorization: Bearer $token")
  code="$(curl -sS -o "$out" -w '%{http_code}' -X POST "$API/bee.v1.$method" \
    -H 'Content-Type: application/json' ${auth[@]+"${auth[@]}"} -d "$body")" \
    || die "$method: the request failed"
  [ "$code" = "200" ] || die "$method answered HTTP $code: $(cat "$out")"
  cat "$out"
}

assert() { # description, then a command that must succeed
  local what=$1
  shift
  "$@" >/dev/null 2>&1 || die "assertion failed: $what"
  say "ok: $what"
}

# An id_token for user "admin" from the mock provider: the authorization-code flow with the
# provider's login form answered by a POST, as a browser would. The issuer name is the compose
# service name, so the requests carry that Host header.
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

oidc_login() { # prints a bee token
  local jwt
  jwt="$(oidc_id_token)"
  rpc AuthService/Login "$(jq -nc --arg t "$jwt" '{oidcIdToken: $t}')" "" | jq -er .token
}

# The bee CLI that ships in the server image, run inside the compose network as the
# invoking user, with $WORK mounted at /work.
bee() {
  docker run --rm --network "${PROJECT}_default" --user "$(id -u):$(id -g)" \
    -e HOME=/work/.home -e BEE_ENDPOINT=http://bee-server:7420 -e "BEE_TOKEN=$TOKEN" \
    -v "$WORK:/work" -w /work --entrypoint bee "$SERVER_IMAGE" "$@"
}

GITHOME="$TMP/githome"
mkdir -p "$GITHOME"
g() { # git without the host's configuration, authenticating with the token
  HOME="$GITHOME" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
    git -c "http.extraHeader=Authorization: Bearer $TOKEN" "$@"
}

b64() { printf %s "$1" | base64 | tr -d '\n'; }
REPO_REF='{"fullName":"acme/app"}'
main_ref() { # prints "<version> <target>" of main
  rpc RefService/ListRefs "{\"repo\":$REPO_REF}" | jq -er '.refs[] | select(.name == "main") | "\(.version) \(.target.raw)"'
}
log_json() { rpc QueryService/Log "{\"repo\":$REPO_REF,\"from\":{\"expr\":\"main\"},\"includeMeta\":true}"; }
blame_json() { rpc QueryService/Blame "{\"repo\":$REPO_REF,\"at\":{\"expr\":\"main\"},\"path\":\"$(b64 "$1")\"}"; }

# Every ref (Bee refs with their versions and targets, and git refs) and every git object
# reachable from them, history, blame and file contents, as one comparable document.
snapshot() {
  local mirror="$TMP/mirror"
  rm -rf "$mirror"
  g clone -q --mirror "$API/acme/app.git" "$mirror"
  {
    rpc RefService/ListRefs "{\"repo\":$REPO_REF}"
    g -C "$mirror" for-each-ref --format='%(objectname) %(refname)' | jq -Rs '{gitRefs: .}'
    g -C "$mirror" rev-list --all --objects | sort | jq -Rs '{gitObjects: .}'
    log_json
    blame_json hello.txt
    bee cat --repo acme/app hello.txt | jq -Rs '{hello: .}'
    bee cat --repo acme/app from-git.txt | jq -Rs '{fromGit: .}'
  } | jq -S .
}
same_snapshot() { # label, before, after
  if [ "$2" != "$3" ]; then
    diff <(echo "$2") <(echo "$3") >&2 || true
    die "refs, objects, history, blame or files differ $1"
  fi
  say "ok: every ref, object, log entry, blame range and file is identical $1 ($(jq -r '.gitObjects' <<<"$2" | grep -c .) git objects, $(jq -s '[.[] | .refs // [] | length] | add' <<<"$2") Bee refs)"
}
fsck_clean() {
  rpc AdminService/RunFsck '{"org":"acme","verifyBytes":true}' | jq -e '(.violations // []) | length == 0'
}

wait_backup() { # id: waits for SUCCEEDED, fails on FAILED
  local id=$1 waited=0 state
  while :; do
    state="$(rpc AdminService/ListBackups '{}' | jq -r --arg id "$id" '.backups[] | select(.id == $id) | .state')"
    [ "$state" = BACKUP_STATE_SUCCEEDED ] && return 0
    [ "$state" != BACKUP_STATE_FAILED ] || die "backup $id failed: $(rpc AdminService/ListBackups '{}' | jq -c --arg id "$id" '.backups[] | select(.id == $id)')"
    [ "$waited" -lt 120 ] || die "backup $id did not succeed within 120s (state $state)"
    sleep 1
    waited=$((waited + 1))
  done
}
run_restore() { # backup-id target-cell drill(true|false): prints the finished restore
  local body id waited=0 r
  body="$(jq -nc --arg id "$1" --arg cell "$2" --argjson drill "$3" '{backupId: $id, targetCell: $cell, drill: $drill}')"
  id="$(rpc AdminService/StartRestore "$body" | jq -er .id)"
  until r="$(rpc AdminService/GetRestore "{\"id\":\"$id\"}")" && [ "$(jq -r .state <<<"$r")" != BACKUP_STATE_RUNNING ]; do
    [ "$waited" -lt 180 ] || die "restore $id did not finish within 180s"
    sleep 1
    waited=$((waited + 1))
  done
  echo "$r"
}

# ---------------------------------------------------------------------------------------------
phase "build images"
# ---------------------------------------------------------------------------------------------
locked() { # runs "$@" under BEE_CHECK_BUILD_LOCK when it is set
  if [ -z "${BEE_CHECK_BUILD_LOCK:-}" ]; then
    "$@"
  elif command -v lockf >/dev/null 2>&1; then
    lockf "$BEE_CHECK_BUILD_LOCK" "$@"
  else
    flock "$BEE_CHECK_BUILD_LOCK" "$@"
  fi
}
locked "${COMPOSE[@]}" build --build-arg "CARGO_BUILD_JOBS=$BUILD_JOBS" \
  --build-arg "BUILD_SHA=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
docker image ls --format '{{.Repository}}:{{.Tag}} {{.Size}}' | grep -E '^(bee-server|bee-web):latest ' | sed "s/^/$NAME: image /"
for img in bee-server:latest bee-web:latest; do
  user="$(docker image inspect --format '{{.Config.User}}' "$img")"
  health="$(docker image inspect --format '{{if .Config.Healthcheck}}yes{{end}}' "$img")"
  assert "$img runs as a non-root user ($user) and has a HEALTHCHECK" \
    test -n "$user" -a "$user" != root -a "$user" != 0 -a "$health" = yes
done

# ---------------------------------------------------------------------------------------------
phase "start stack"
# ---------------------------------------------------------------------------------------------
"${COMPOSE[@]}" up -d --wait --wait-timeout "$TIMEOUT_SECS"
"${COMPOSE[@]}" ps --format '{{.Service}} {{.Status}}' | sed "s/^/$NAME:   /"
wait_for_code "server /readyz" "$API/readyz" 200
wait_for_code "web /" "$WEB/" 200
wait_for_code "mock OIDC discovery" "http://127.0.0.1:$OIDC_PORT/default/.well-known/openid-configuration" 200
INFO="$(rpc ObjectService/GetServerInfo '{}' "")"
assert "the server reports the enterprise edition" test "$(jq -r .edition <<<"$INFO")" = enterprise

# ---------------------------------------------------------------------------------------------
phase "first admin"
# ---------------------------------------------------------------------------------------------
TOKEN="$(oidc_login)"
ADMIN_ID="$(rpc AuthService/WhoAmI '{}' | jq -er .principal.id)"
say "first sign-in made principal $ADMIN_ID"
assert "a new sign-in is not yet an operator (CreateBackup is refused)" \
  test "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/bee.v1.AdminService/CreateBackup" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d '{}')" != 200
# Written in place so the bind-mounted file keeps its inode.
sed "s|^site_admins = \[\]|site_admins = [\"$ADMIN_ID\"]|" "$SERVER_TOML" > "$TMP/server.toml.new"
grep -q "site_admins = \[\"$ADMIN_ID\"\]" "$TMP/server.toml.new" || die "deploy/server.toml has no 'site_admins = []' line to edit"
cat "$TMP/server.toml.new" > "$SERVER_TOML"
"${COMPOSE[@]}" up -d --wait --wait-timeout "$TIMEOUT_SECS" --force-recreate --no-deps bee-init bee-server
wait_for_code "server /readyz" "$API/readyz" 200
TOKEN="$(oidc_login)"
assert "the same principal signed in again" test "$(rpc AuthService/WhoAmI '{}' | jq -er .principal.id)" = "$ADMIN_ID"

# ---------------------------------------------------------------------------------------------
phase "org, repo, land with the CLI"
# ---------------------------------------------------------------------------------------------
rpc OrgService/CreateOrg '{"name":"acme","display":"Acme"}' >/dev/null
REPO="$(rpc RepoService/CreateRepo '{"org":"acme","name":"app","visibility":"VISIBILITY_PUBLIC","description":"Self-host check","autoInit":true}')"
assert "the repo is acme/app" test "$(jq -r .fullName <<<"$REPO")" = acme/app
bee clone acme/app wc >/dev/null
printf 'hello from the bee CLI\nsecond line\n' > "$WORK/wc/hello.txt"
bee -C /work/wc commit -m "Add hello.txt" --json >/dev/null
LAND="$(bee -C /work/wc land --json)"
assert "the CLI landed one change on main" test "$(jq -r '.data.land.landed | length' <<<"$LAND")" = 1
assert "bee cat reads the landed file" test "$(bee cat --repo acme/app hello.txt)" = "$(printf 'hello from the bee CLI\nsecond line')"
read -r V_AFTER_CLI _ <<<"$(main_ref)"

# ---------------------------------------------------------------------------------------------
phase "git clone and push"
# ---------------------------------------------------------------------------------------------
g clone -q "$API/acme/app.git" "$TMP/gitwc"
assert "git clone has README.md and hello.txt" test -f "$TMP/gitwc/README.md" -a -f "$TMP/gitwc/hello.txt"
assert "git clone has the change landed by the CLI" \
  test "$(g -C "$TMP/gitwc" log --format=%s | grep -c '^Add hello.txt')" = 1
echo "pushed with git" > "$TMP/gitwc/from-git.txt"
g -C "$TMP/gitwc" add from-git.txt
g -C "$TMP/gitwc" -c user.name="Self Host" -c user.email=selfhost@example.com commit -q -m "Add from-git.txt"
g -C "$TMP/gitwc" push -q origin HEAD:main
read -r V_AFTER_PUSH _ <<<"$(main_ref)"
assert "the push advanced main (version $V_AFTER_CLI -> $V_AFTER_PUSH)" test "$V_AFTER_PUSH" -gt "$V_AFTER_CLI"
assert "bee cat reads the pushed file" test "$(bee cat --repo acme/app from-git.txt)" = "pushed with git"
g clone -q "$API/acme/app.git" "$TMP/gitwc2"
assert "a second clone has both files" test -f "$TMP/gitwc2/hello.txt" -a -f "$TMP/gitwc2/from-git.txt"
assert "git over SSH answers on port $SSH_PORT" \
  test "$(bash -c "exec 3<>/dev/tcp/127.0.0.1/$SSH_PORT; head -c 4 <&3")" = "SSH-"

# ---------------------------------------------------------------------------------------------
phase "log, blame, diff"
# ---------------------------------------------------------------------------------------------
LOG="$(log_json)"
assert "log lists the pushed, landed and initial changes, newest first" \
  jq -e '[.changesets[].meta.message // ""] | (.[0] | startswith("Add from-git.txt")) and (.[1] == "Add hello.txt") and (.[2] == "Initial commit")' <<<"$LOG"
HELLO_CHANGESET="$(jq -er '.changesets[] | select(.meta.message == "Add hello.txt") | .id.raw' <<<"$LOG")"
assert "blame attributes both lines of hello.txt to the change that added it" \
  jq -e --arg c "$HELLO_CHANGESET" '(.ranges | length) == 1 and .ranges[0].lineCount == 2 and .ranges[0].changeset.raw == $c' <<<"$(blame_json hello.txt)"
DIFF="$(bee diff --repo acme/app --from 'main~1' --to main --json)"
assert "diff of the last change adds from-git.txt with one line" \
  jq -e '(.data.files | length) == 1 and .data.files[0].path == "from-git.txt" and .data.files[0].status == "added" and .data.files[0].additions == 1' <<<"$DIFF"

# ---------------------------------------------------------------------------------------------
phase "web app"
# ---------------------------------------------------------------------------------------------
page() { curl -sL -o "$TMP/page.html" -w '%{http_code}' "$@"; }
assert "web home answers 200" test "$(page "$WEB/")" = 200
assert "the public repo page answers 200 and names acme/app" \
  test "$(page "$WEB/acme/app")" = 200 -a "$(grep -c 'acme/app' "$TMP/page.html")" -ge 1
assert "signed in (session cookie), the admin console answers 200" \
  test "$(page -b "bee_token=$TOKEN" "$WEB/admin")" = 200 -a "$(grep -c '<title>Admin' "$TMP/page.html")" -ge 1
assert "signed out, the admin console sends the visitor to sign-in" \
  test "$(curl -sL -o /dev/null -w '%{url_effective}' "$WEB/admin")" = "$WEB/signin?next=%2Fadmin"
assert "the web app serves /llms.txt" test "$(page "$WEB/llms.txt")" = 200

# ---------------------------------------------------------------------------------------------
phase "backup to S3 and drill"
# ---------------------------------------------------------------------------------------------
BACKUP_ID="$(rpc AdminService/CreateBackup '{"note":"check-selfhost"}' | jq -er .id)"
wait_backup "$BACKUP_ID"
assert "ListBackups shows backup $BACKUP_ID as succeeded and non-empty" \
  jq -e --arg id "$BACKUP_ID" '[.backups[] | select(.id == $id and (.sizeBytes | tonumber) > 0)] | length == 1' <<<"$(rpc AdminService/ListBackups '{}')"
s3_count() { # objects under a prefix of the bucket
  "${COMPOSE[@]}" run --rm --no-deps --entrypoint aws s3-init --endpoint-url http://s3:9000 \
    s3 ls "s3://bee/$1" --recursive 2>/dev/null | wc -l | tr -d ' '
}
BACKUP_OBJECTS="$(s3_count bee-backups/)"
BLOB_OBJECTS="$(s3_count blobs/)"
say "the bucket holds $BACKUP_OBJECTS backup objects and $BLOB_OBJECTS blob objects"
assert "backup objects are in the bucket under bee-backups/" test "$BACKUP_OBJECTS" -gt 0
assert "blob objects are in the bucket under blobs/" test "$BLOB_OBJECTS" -gt 0
DRILL="$(run_restore "$BACKUP_ID" drill1 true)"
say "drill: $(jq -r .detail <<<"$DRILL")"
assert "the restore drill succeeded with a clean fsck and passing drill checks" \
  jq -e '.state == "BACKUP_STATE_SUCCEEDED" and .fsckClean == true and .conformancePassed == true' <<<"$DRILL"
assert "the drill wrote its data directory into the server volume" \
  "${COMPOSE[@]}" exec -T bee-server test -f /data/restores/drill1/restore.json
assert "the drill created the Postgres schemas restore_drill1 and restore_drill1_forge" \
  test "$("${COMPOSE[@]}" exec -T postgres psql -U bee -d bee -Atc \
    "select count(*) from information_schema.schemata where schema_name in ('restore_drill1', 'restore_drill1_forge')")" = 2
BEFORE="$(snapshot)"
assert "fsck of org acme is clean before the disaster" fsck_clean

# ---------------------------------------------------------------------------------------------
phase "restore into a fresh stack"
# ---------------------------------------------------------------------------------------------
# The disaster: the database and the server's data volume are gone. The bucket survives (on real
# object storage it is replicated; here its volume is kept), and with it the backups.
"${COMPOSE[@]}" down
docker volume rm "${PROJECT}_postgres-data" "${PROJECT}_bee-data" >/dev/null
assert "the Postgres and data volumes are gone" \
  bash -c "! docker volume inspect ${PROJECT}_postgres-data >/dev/null 2>&1 && ! docker volume inspect ${PROJECT}_bee-data >/dev/null 2>&1"
# The operator keeps deploy/.env and deploy/server.toml (signing key, site_admins) and starts a
# fresh stack: empty database, empty data volume.
"${COMPOSE[@]}" up -d --wait --wait-timeout "$TIMEOUT_SECS"
wait_for_code "server /readyz" "$API/readyz" 200
assert "the fresh stack has no org acme" \
  test "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/bee.v1.RepoService/GetRepo" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d "{\"repo\":$REPO_REF}")" != 200
assert "the fresh stack lists backup $BACKUP_ID from the bucket" \
  jq -e --arg id "$BACKUP_ID" '[.backups[] | select(.id == $id and .state == "BACKUP_STATE_SUCCEEDED")] | length == 1' <<<"$(rpc AdminService/ListBackups '{}')"
RESTORE="$(run_restore "$BACKUP_ID" recovered true)"
say "restore: $(jq -r .detail <<<"$RESTORE")"
assert "the restore succeeded with a clean fsck and passing drill checks" \
  jq -e '.state == "BACKUP_STATE_SUCCEEDED" and .fsckClean == true and .conformancePassed == true' <<<"$RESTORE"
# Point the server at the restored data (docs/self-hosting.md, "Backup and restore"): the
# metadata schema, the forge schema and the data directory the restore wrote.
sed -e 's|^postgres_schema = .*|postgres_schema = "restore_recovered"|' \
  -e 's|^data_dir = .*|data_dir = "/data/restores/recovered"|' "$SERVER_TOML" > "$TMP/server.toml.new"
cat "$TMP/server.toml.new" > "$SERVER_TOML"
sed -e 's|schema=bee_forge|schema=restore_recovered_forge|' "$ENV_FILE" > "$TMP/env.new"
grep -q 'schema=restore_recovered_forge' "$TMP/env.new" || die "deploy/.env.example has no forge schema to point at the restore"
cat "$TMP/env.new" > "$ENV_FILE"
"${COMPOSE[@]}" up -d --wait --wait-timeout "$TIMEOUT_SECS" --force-recreate --no-deps bee-init bee-server web
wait_for_code "server /readyz" "$API/readyz" 200
assert "the token issued before the disaster still verifies, for the same principal" \
  test "$(rpc AuthService/WhoAmI '{}' | jq -er .principal.id)" = "$ADMIN_ID"
same_snapshot "after restoring into a fresh stack" "$BEFORE" "$(snapshot)"
assert "fsck of org acme (re-hashing blob bytes) is clean after the restore" fsck_clean
assert "the restored repo page answers 200 and names acme/app" \
  test "$(page "$WEB/acme/app")" = 200 -a "$(grep -c 'acme/app' "$TMP/page.html")" -ge 1
echo "landed after the restore" > "$TMP/gitwc/after-restore.txt"
g -C "$TMP/gitwc" add after-restore.txt
g -C "$TMP/gitwc" -c user.name="Self Host" -c user.email=selfhost@example.com commit -q -m "Add after-restore.txt"
g -C "$TMP/gitwc" push -q origin HEAD:main
assert "the restored server takes a new push" test "$(bee cat --repo acme/app after-restore.txt)" = "landed after the restore"

# ---------------------------------------------------------------------------------------------
phase "restart survives"
# ---------------------------------------------------------------------------------------------
BEFORE="$(snapshot)"
"${COMPOSE[@]}" restart bee-server
wait_for_code "server /readyz" "$API/readyz" 200
same_snapshot "after restarting the server container" "$BEFORE" "$(snapshot)"

# ---------------------------------------------------------------------------------------------
phase "caddy profile (TLS front)"
# ---------------------------------------------------------------------------------------------
BEE_DOMAIN=localhost "${COMPOSE[@]}" --profile caddy up -d --wait --no-deps caddy
waited=0
until [ "$(curl -ks -o /dev/null -w '%{http_code}' "https://localhost:$HTTPS_PORT/healthz" || true)" = 200 ]; do
  [ "$waited" -lt 60 ] || die "caddy did not serve https://localhost:$HTTPS_PORT/healthz within 60s"
  sleep 1
  waited=$((waited + 1))
done
assert "caddy sends /healthz to the server" test "$(curl -ks "https://localhost:$HTTPS_PORT/healthz")" = ok
assert "caddy sends RPCs to the server" \
  test "$(curl -ks -X POST "https://localhost:$HTTPS_PORT/bee.v1.AuthService/WhoAmI" -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $TOKEN" -d '{}' | jq -r .principal.id)" = "$ADMIN_ID"
assert "caddy sends git smart HTTP to the server" \
  test "$(g -c http.sslVerify=false ls-remote "https://localhost:$HTTPS_PORT/acme/app.git" HEAD | wc -l | tr -d ' ')" = 1
assert "caddy sends repo pages to the web app" \
  test "$(page -k "https://localhost:$HTTPS_PORT/acme/app")" = 200 -a "$(grep -c '<title>acme/app' "$TMP/page.html")" -ge 1
assert "caddy does not forward /metrics" \
  test "$(page -k "https://localhost:$HTTPS_PORT/metrics")" != 200 -o "$(grep -c bee_ "$TMP/page.html")" = 0
"${COMPOSE[@]}" --profile caddy rm -sf caddy >/dev/null 2>&1

# ---------------------------------------------------------------------------------------------
phase "production checklist"
# ---------------------------------------------------------------------------------------------
check_production() { # extra docker compose run arguments
  "${COMPOSE[@]}" run --rm --no-deps "$@" bee-init --config /etc/bee/server.toml --check-production 2>&1
}
set +e
HTTP_REPORT="$(check_production)"
HTTP_STATUS=$?
HTTPS_REPORT="$(check_production -e BEE_PUBLIC_URL=https://bee.example.com)"
HTTPS_STATUS=$?
set -e
grep -E '^(ok|warn|FAIL|production)' <<<"$HTTP_REPORT" | sed "s/^/$NAME:   /"
assert "with the plain-http test address only the tls item fails" \
  test "$HTTP_STATUS" = 1 -a "$(grep -c '^FAIL' <<<"$HTTP_REPORT")" = 1 -a "$(grep -c '^FAIL tls' <<<"$HTTP_REPORT")" = 1
assert "with an https public_url behind the proxy the production checklist passes" \
  test "$HTTPS_STATUS" = 0 -a "$(grep -c '^production checklist: ok' <<<"$HTTPS_REPORT")" = 1

phase "teardown"
"${COMPOSE[@]}" --profile caddy down --volumes --remove-orphans >/dev/null 2>&1
TORN_DOWN=1
phase ""
print_timings
echo "$NAME: ok"
