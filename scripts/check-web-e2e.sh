#!/usr/bin/env bash
# Playwright end-to-end and accessibility tests for apps/web, against a real bee-server.
#
#   1. builds bee-server (debug) and the web UI, once;
#   2. for each edition (enterprise, then cloud): starts `--dev --config` on a free port with a
#      temporary data dir and that `[server] edition`, seeds it through the public API
#      (apps/web/e2e/seed.ts, generated bee.v1 clients) and runs every Playwright spec
#      (flows + axe) against `next start` pointed at that server. Tests tagged `@cloud` or
#      `@enterprise` run only in that edition (playwright.config.ts); screenshots land in
#      apps/web/e2e/screenshots (gitignored);
#   3. runs Lighthouse on the change view when a `lighthouse` binary is on PATH.
#
# Environment: E2E_SKIP_BUILD=1 reuses .next; E2E_KEEP=1 keeps the data dirs;
# E2E_EDITIONS="cloud" (or "enterprise") runs one edition; BUN, CARGO as usual.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WEB="$ROOT/apps/web"
BUN="${BUN:-$(command -v bun || echo "$HOME/.bun/bin/bun")}"
EDITIONS="${E2E_EDITIONS:-enterprise cloud}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-line-tables-only}"

free_port() { node -e 'const s=require("net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})'; }

echo "== building bee-server"
(cd "$ROOT" && cargo build -q -p bee-server)
# cargo puts the binary under CARGO_TARGET_DIR when it is set (a shared target across worktrees).
SERVER="${CARGO_TARGET_DIR:-$ROOT/target}/debug/bee-server"

DATA=""
SERVER_PID=""
stop_server() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi
  SERVER_PID=""
  if [ -n "$DATA" ]; then
    if [ -z "${E2E_KEEP:-}" ]; then rm -rf "$DATA"; else echo "kept $DATA"; fi
  fi
  DATA=""
}
trap stop_server EXIT

# Starts bee-server --dev for one edition and seeds it; sets DATA, SERVER_PID and API_PORT.
start_server() {
  local edition="$1"
  DATA="$(mktemp -d "${TMPDIR:-/tmp}/bee-web-e2e.XXXXXX")"
  API_PORT="$(free_port)"
  # With --config, --dev takes only the data dir and import roots from dev mode, so the file
  # repeats the rest of what --dev sets (Config::dev).
  printf '[server]\nedition = "%s"\ncell = "dev"\n\n[forge]\npages_memory_dns = true\n' "$edition" > "$DATA/server.toml"
  echo "== [$edition] starting bee-server --dev on 127.0.0.1:$API_PORT (data $DATA)"
  "$SERVER" --dev --config "$DATA/server.toml" --listen "127.0.0.1:$API_PORT" --data-dir "$DATA/server" > "$DATA/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 300); do
    [ -f "$DATA/server/credentials" ] && break
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then cat "$DATA/server.log"; echo "bee-server exited" >&2; exit 1; fi
    sleep 0.1
  done
  [ -f "$DATA/server/credentials" ] || { cat "$DATA/server.log"; echo "bee-server did not write credentials" >&2; exit 1; }
  echo "== [$edition] seeding through the public API"
  (cd "$WEB" && "$BUN" e2e/seed.ts "$DATA/server")
}

cd "$WEB"
"$BUN" install --frozen-lockfile >/dev/null
if [ -z "${E2E_SKIP_BUILD:-}" ]; then
  echo "== building the web UI"
  # Next prerendering needs production React even when the sandbox exports development NODE_ENV.
  NODE_ENV=production "$BUN" run build >/dev/null
fi
mkdir -p e2e/screenshots

LAST="${EDITIONS##* }"
for EDITION in $EDITIONS; do
  start_server "$EDITION"
  export BEE_API_URL="http://127.0.0.1:$API_PORT"
  export E2E_DATA_DIR="$DATA/server"
  E2E_PORT="$(free_port)"
  export E2E_PORT
  echo "== [$EDITION] playwright against $BEE_API_URL"
  E2E_EDITION="$EDITION" E2E_SKIP_BUILD=1 "$BUN" run e2e
  echo "== [$EDITION] ok"
  # Lighthouse below runs against the last edition's server, so that one stays up.
  [ "$EDITION" = "$LAST" ] || stop_server
done

if command -v lighthouse >/dev/null 2>&1; then
  echo "== lighthouse (change view)"
  node e2e/lighthouse.mjs
else
  echo "== lighthouse: skipped (no lighthouse binary on PATH)"
fi
echo "check-web-e2e: ok ($EDITIONS)"
