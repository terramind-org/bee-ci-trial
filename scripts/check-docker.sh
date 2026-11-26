#!/usr/bin/env bash
# Builds the server and web images, starts the development stack (deploy/compose.dev.yml) as
# compose project `selfhost-dev`, checks what docs/self-hosting.md ("Quick start") promises (the
# server and the web app answer, the dev token in /data/credentials signs in, dev/playground
# exists), validates deploy/server.toml inside the image and tears everything down with its
# volumes. The production stack has its own check: scripts/check-selfhost.sh.
#
# Not part of scripts/check.sh: it needs a Docker daemon and several minutes. Without docker it
# skips with exit 0, unless CI=true.
# Host ports, all non-default so a running dev server does not collide: 17520 (server),
# 13200 (web), 12322 (ssh); override with BEE_CHECK_{API,WEB,SSH}_PORT.
# BEE_CHECK_BUILD_LOCK=<file> runs the image build under `lockf <file>` (or `flock` on Linux),
# for machines where heavy builds share one lock.
set -euo pipefail

cd "$(dirname "$0")/.."
NAME=check-docker

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  if [ "${CI:-}" = "true" ]; then
    echo "$NAME: docker is required under CI=true but is not installed or its daemon is not running" >&2
    exit 1
  fi
  echo "$NAME: docker is not installed or its daemon is not running; skipping"
  exit 0
fi
for tool in curl jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    if [ "${CI:-}" = "true" ]; then
      echo "$NAME: $tool is required under CI=true" >&2
      exit 1
    fi
    echo "$NAME: $tool is not installed; skipping"
    exit 0
  fi
done

export BEE_API_PORT="${BEE_CHECK_API_PORT:-17520}"
export BEE_WEB_PORT="${BEE_CHECK_WEB_PORT:-13200}"
export BEE_SSH_PORT="${BEE_CHECK_SSH_PORT:-12322}"
COMPOSE=(docker compose -p selfhost-dev -f deploy/compose.dev.yml)
SERVER_URL="http://127.0.0.1:$BEE_API_PORT"
WEB_URL="http://127.0.0.1:$BEE_WEB_PORT"
TIMEOUT_SECS="${BEE_CHECK_TIMEOUT_SECS:-180}"
START_TIME=$SECONDS

say() { echo "$NAME: $*"; }
die() { echo "$NAME: FAIL: $*" >&2; exit 1; }

cleanup() {
  status=$?
  trap - EXIT
  if [ "$status" -ne 0 ]; then
    echo "$NAME: failed; container logs follow" >&2
    "${COMPOSE[@]}" logs --no-color --tail 80 >&2 || true
  fi
  "${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
"${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
for p in "$BEE_API_PORT" "$BEE_WEB_PORT" "$BEE_SSH_PORT"; do
  ! port_busy "$p" || die "host port $p is already in use; stop whatever listens on it"
done

wait_for() { # name url (redirects are followed: the web app sends / to its landing page)
  local name=$1 url=$2 waited=0
  until [ "$(curl -sL -o /dev/null -w '%{http_code}' "$url" || true)" = "200" ]; do
    [ "$waited" -lt "$TIMEOUT_SECS" ] || die "$name did not answer 200 at $url within ${TIMEOUT_SECS}s"
    sleep 2
    waited=$((waited + 2))
  done
  say "$name is up ($url)"
}

locked() { # runs "$@" under BEE_CHECK_BUILD_LOCK when it is set
  if [ -z "${BEE_CHECK_BUILD_LOCK:-}" ]; then
    "$@"
  elif command -v lockf >/dev/null 2>&1; then
    lockf "$BEE_CHECK_BUILD_LOCK" "$@"
  else
    flock "$BEE_CHECK_BUILD_LOCK" "$@"
  fi
}

say "building images"
T=$SECONDS
locked "${COMPOSE[@]}" build
BUILD_SECS=$((SECONDS - T))

say "validating deploy/server.toml with --check-config"
docker run --rm \
  -e BEE_PUBLIC_URL=https://bee.example.com \
  -e BEE_POSTGRES_URL=postgres://bee:x@postgres:5432/bee \
  -e BEE_FORGE_DATABASE='postgres://bee:x@postgres:5432/bee?schema=bee_forge' \
  -e BEE_S3_ENDPOINT=http://s3:9000 \
  -e BEE_S3_BUCKET=bee \
  -e BEE_S3_ACCESS_KEY=access \
  -e BEE_S3_SECRET_KEY=secretsecret \
  -e BEE_SIGNING_KEY_HEX=0000000000000000000000000000000000000000000000000000000000000001 \
  -v "$PWD/deploy/server.toml:/etc/bee/server.toml:ro" \
  bee-server:dev --config /etc/bee/server.toml --check-config

say "starting the dev stack"
T=$SECONDS
"${COMPOSE[@]}" up -d
wait_for "server /healthz" "$SERVER_URL/healthz"
wait_for "server /readyz" "$SERVER_URL/readyz"
wait_for "web" "$WEB_URL/"
START_SECS=$((SECONDS - T))

# The quick start tells people to read the dev token from /data/credentials.
TOKEN="$("${COMPOSE[@]}" exec -T bee-server cat /data/credentials | sed -n 's/^token = "\(.*\)"$/\1/p')"
[ -n "$TOKEN" ] || die "/data/credentials has no token line"
rpc() {
  curl -fsS -X POST "$SERVER_URL/bee.v1.$1" -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $TOKEN" -d "$2"
}
WHO="$(rpc AuthService/WhoAmI '{}')" || die "the token in /data/credentials does not sign in"
jq -e '.principal.id != "" and .principal.kind == "PRINCIPAL_KIND_HUMAN"' <<<"$WHO" >/dev/null \
  || die "the token in /data/credentials does not name a human principal: $WHO"
say "ok: the token in /data/credentials signs in ($(jq -r .principal.display <<<"$WHO"))"
[ "$(rpc RepoService/GetRepo '{"repo":{"fullName":"dev/playground"}}' | jq -r .fullName)" = dev/playground ] \
  || die "dev mode did not create dev/playground"
say "ok: dev mode created dev/playground"
# dev/playground is private: signed in (the session cookie that pasting the token sets) the web
# app shows it, signed out it does not exist.
PAGE="$(mktemp)"
CODE="$(curl -sL -o "$PAGE" -w '%{http_code}' -b "bee_token=$TOKEN" "$WEB_URL/dev/playground")"
TITLED="$(grep -c '<title>dev/playground' "$PAGE" || true)"
rm -f "$PAGE"
if [ "$CODE" != 200 ] || [ "$TITLED" -lt 1 ]; then
  die "signed in, the web app answered $CODE for /dev/playground (title found: $TITLED)"
fi
say "ok: signed in, the web app serves the private repo dev/playground"
CODE="$(curl -sL -o /dev/null -w '%{http_code}' "$WEB_URL/dev/playground")"
[ "$CODE" = 404 ] || die "signed out, the web app answered $CODE for the private repo dev/playground"
say "ok: signed out, the private repo dev/playground is not found"

say "timings: build ${BUILD_SECS}s, start to healthy ${START_SECS}s, total $((SECONDS - START_TIME))s"
echo "$NAME: ok"
