#!/usr/bin/env bash
# The PLAN phase 1 exit test, as one script, with no LLM involved:
#
#   1. `bee-github import terramind-org/bee` into a `bee-server --dev` instance: every
#      commit keeps its git id, the fidelity report has no unexplained drops;
#   2. a change made with an agent token through `bee mcp` (scripted JSON-RPC) and landed;
#   3. `git clone` against the server and a `git push` that lands under policy, and one that
#      policy refuses with POLICY_DENIED;
#   4. log, diff and blame over the API, asserted on the content of steps 2 and 3;
#   5. the SPEC §16 load harness, with its report and every missed target;
#   6. a new server process on the same data directory, with fresh clone and API assertions.
#
# Prints a summary table and ends with `exit-test: ok` only when all six steps passed.
#
# Environment (docs/e2e.md has the full list):
#   GITHUB_TOKEN / GH_TOKEN     GitHub token that can read the source repo (else `gh auth token`)
#   EXIT_TEST_REPO              source repository, owner/name (default terramind-org/bee)
#   EXIT_TEST_SKIP_IMPORT=1     offline mode: skip the import; step 1 is INCOMPLETE, the run
#                               cannot pass and exits 3
#   EXIT_TEST_PROFILE           release (default) or dev (debug-profile scenario run)
#   EXIT_TEST_LOAD_ARGS         flags for the load harness (default in step5-load.ts)
#   EXIT_TEST_KEEP=1            keep the scratch directory
#   CARGO_BUILD_JOBS, CARGO_TARGET_DIR, BUN as usual
set -euo pipefail
umask 077

EXIT_TEST_PROFILE="${EXIT_TEST_PROFILE:-release}"
case "$EXIT_TEST_PROFILE" in
  release) BUILD_PROFILE_ARGS=(--release); TARGET_PROFILE=release ;;
  dev) BUILD_PROFILE_ARGS=(); TARGET_PROFILE=debug; echo "profile: dev (load figures are not performance measurements)" ;;
  *) echo "exit-test: EXIT_TEST_PROFILE must be release or dev" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/scripts/e2e"
STEP_NAMES=("import github history" "agent change over MCP" "git clone and push under policy" "log, diff and blame" "load harness (SPEC §16)" "restart, clone and API history")
STEP_FILES=(step1-import.ts step2-mcp.ts step3-git.ts step4-verify.ts step5-load.ts step6-restart.ts)

BUN="${BUN:-$(command -v bun || echo "$HOME/.bun/bin/bun")}"
need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  if [ "${CI:-}" = "true" ]; then echo "exit-test: $1 is required" >&2; exit 1; fi
  echo "exit-test: skipped, $1 is required and not installed"; exit 0
}
need git; need cargo; need protoc
[ -x "$BUN" ] || need bun
if [ -z "${EXIT_TEST_SKIP_IMPORT:-}" ] && [ -z "${GITHUB_TOKEN:-}${GH_TOKEN:-}" ]; then need gh; fi

export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
# Serialize heavy builds with every other launch worker on this machine.
if [ "$(uname -s)" = Darwin ]; then
  need lockf
  BUILD_LOCK=(lockf /tmp/bee-build.lock)
  DISK=/System/Volumes/Data
else
  need flock
  BUILD_LOCK=(flock /tmp/bee-build.lock)
  DISK="$ROOT"
fi
df -h "$DISK"
FREE_KIB="$(df -k "$DISK" | awk 'END {print $4}')"
if [ "$FREE_KIB" -lt 10485760 ]; then
  echo "exit-test: less than 10 GiB free; refusing a heavy build" >&2
  exit 1
fi
free_port() { "$BUN" -e 'const s=Bun.listen({hostname:"127.0.0.1",port:0,socket:{data(){}}});console.log(s.port);s.stop()'; }
now() { date +%s; }

START="$(now)"
uname -sm
uptime
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/bee-exit-test.XXXXXX")"
SERVER_PID=""
# Mirror mode follows GitHub's refs. This test imports a snapshot, then makes local lands;
# leave live-sync polling to its own drill so a 300s poll cannot replace those local refs.
cat > "$WORK/server.toml" <<'CONFIG'
[server]
cell = "dev"
[forge]
pages_memory_dns = true
[import]
github_sync_interval_secs = 0
CONFIG
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi
  if [ -n "${EXIT_TEST_KEEP:-}" ]; then echo "exit-test: kept $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

echo "== building bee, bee-server, bee-github and the load harness ($EXIT_TEST_PROFILE)"
(cd "$ROOT" && "${BUILD_LOCK[@]}" cargo build "${BUILD_PROFILE_ARGS[@]}" --locked -q -p bee-cli -p bee-server -p bee-github \
  && "${BUILD_LOCK[@]}" cargo build "${BUILD_PROFILE_ARGS[@]}" --locked -q -p bee-conformance --example load)
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET" in /*) ;; *) TARGET="$ROOT/$TARGET" ;; esac
TARGET="$TARGET/$TARGET_PROFILE"
for b in bee bee-server bee-github examples/load; do
  [ -x "$TARGET/$b" ] || { echo "exit-test: $TARGET/$b was not built" >&2; exit 1; }
done

PORT="$(free_port)"
echo "== starting bee-server --dev --temp on 127.0.0.1:$PORT"
"$TARGET/bee-server" --dev --temp --config "$WORK/server.toml" --data-dir "$WORK/server" --listen "127.0.0.1:$PORT" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
CREDS=""
for _ in $(seq 600); do
  CREDS="$(sed -n 's/^credentials written to //p' "$WORK/server.log" 2>/dev/null | sed -n 1p)"
  [ -n "$CREDS" ] && [ -f "$CREDS" ] && break
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then echo "exit-test: see $WORK/server.log for private diagnostics"; echo "exit-test: bee-server exited" >&2; exit 1; fi
  sleep 0.1
done
if [ -z "$CREDS" ] || [ ! -f "$CREDS" ]; then
  echo "exit-test: see $WORK/server.log for private diagnostics"
  echo "exit-test: bee-server wrote no credentials" >&2
  exit 1
fi
# The dev credentials file is TOML: endpoint = "...", token = "...".
EXIT_TEST_ADMIN_TOKEN="$(sed -n 's/^token = "\(.*\)"/\1/p' "$CREDS")"
export EXIT_TEST_ADMIN_TOKEN
export EXIT_TEST_ENDPOINT="http://127.0.0.1:$PORT"
export EXIT_TEST_WORK="$WORK"
export EXIT_TEST_BEE="$TARGET/bee"
export EXIT_TEST_BEE_GITHUB="$TARGET/bee-github"
export EXIT_TEST_LOAD_BIN="$TARGET/examples/load"
export BEE_CONFIG_DIR="$WORK/config"
# Git must not read the user's configuration: credential helpers, hooks paths and url rewrites.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_TERMINAL_PROMPT=0
echo '{}' > "$WORK/state.json"
echo "   ready: $EXIT_TEST_ENDPOINT"

STATUS=(PENDING PENDING PENDING PENDING PENDING PENDING)
SECS=(0 0 0 0 0 0)
failed=0; incomplete=0

# Stop the first process and reopen the exact SQLite/object-store directory in a new one.
restart_server() {
  kill "$SERVER_PID" || return 1
  wait "$SERVER_PID" || true
  "$TARGET/bee-server" --dev --config "$WORK/server.toml" --data-dir "$WORK/server" --listen "127.0.0.1:$PORT" > "$WORK/restarted.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 600); do
    if grep -q '^credentials written to ' "$WORK/restarted.log"; then
      EXIT_TEST_ADMIN_TOKEN="$(sed -n 's/^token = "\(.*\)"/\1/p' "$CREDS")"
      export EXIT_TEST_ADMIN_TOKEN
      return 0
    fi
    kill -0 "$SERVER_PID" 2>/dev/null || { echo "exit-test: restarted server exited (see restarted.log)" >&2; return 1; }
    sleep 0.1
  done
  echo "exit-test: restarted server readiness timed out" >&2
  return 1
}

# Runs step $1 (0-based) and records PASS, FAIL or INCOMPLETE (exit status 3).
run_step() {
  local i="$1" rc t0
  echo
  echo "== step $((i + 1)): ${STEP_NAMES[$i]}"
  if [ "$i" -eq 4 ] && [ "$EXIT_TEST_PROFILE" = dev ]; then
    echo "   performance thresholds: not enforced in dev profile"
  fi
  t0="$(now)"
  set +e
  if [ "$i" -eq 5 ]; then
    restart_server && "$BUN" "$HERE/${STEP_FILES[$i]}"
  else
    "$BUN" "$HERE/${STEP_FILES[$i]}"
  fi
  rc=$?
  set -e
  SECS[i]=$(( $(now) - t0 ))
  case "$rc" in
    0) STATUS[i]=PASS ;;
    3) STATUS[i]=INCOMPLETE; incomplete=1 ;;
    *) STATUS[i]=FAIL; failed=1 ;;
  esac
}

# Steps 1-4 build on each other: after a failure the rest are skipped (and count as failed).
# The load harness (step 5) is independent and always runs.
blocked=0
for i in 0 1 2 3; do
  if [ "$blocked" -eq 1 ]; then
    STATUS[i]=SKIPPED; failed=1
    echo; echo "== step $((i + 1)): ${STEP_NAMES[$i]}: skipped, an earlier step failed"
    continue
  fi
  run_step "$i"
  [ "${STATUS[$i]}" = FAIL ] && blocked=1
done
run_step 4
if [ "$blocked" -eq 0 ]; then run_step 5; else STATUS[5]=SKIPPED; failed=1; fi

echo
echo "== summary"
if [ "$EXIT_TEST_PROFILE" = dev ]; then echo "profile: dev (load figures are not performance measurements)"; fi
printf '%-6s %-36s %-11s %s\n' step name result seconds
for i in 0 1 2 3 4 5; do
  printf '%-6s %-36s %-11s %s\n' "$((i + 1))" "${STEP_NAMES[$i]}" "${STATUS[$i]}" "${SECS[$i]}"
done
TOTAL=$(( $(now) - START ))
echo "total time: ${TOTAL}s ($((TOTAL / 60))m $((TOTAL % 60))s)"
if [ "$failed" -ne 0 ]; then echo "exit-test: FAILED"; exit 1; fi
if [ "$incomplete" -ne 0 ]; then echo "exit-test: INCOMPLETE (a documented offline mode ran; this is not a pass)"; exit 3; fi
echo "exit-test: ok"
