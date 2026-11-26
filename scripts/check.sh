#!/usr/bin/env bash
# Full local gate: formatting, lints, tests, crate layers and the protobuf schema.
set -euo pipefail
cd "$(dirname "$0")/.."
# First python3 that actually runs (a PATH entry can point at an interpreter for another CPU).
PY="${PYTHON:-}"
for c in python3 /usr/bin/python3; do [ -n "$PY" ] && break; "$c" -c "" 2>/dev/null && PY="$c"; done
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
"$PY" scripts/check-layers.py
"$PY" scripts/sync-pins.py --check
"$PY" scripts/check-coverage.py
protoc -I proto --descriptor_set_out=/dev/null --include_imports proto/bee/v1/*.proto
# Web UI: lint, typecheck, unit tests (Playwright e2e runs separately: scripts/check-web-e2e.sh).
if [ -d apps/web ]; then
  BUN="${BUN:-$(command -v bun || echo "$HOME/.bun/bin/bun")}"
  (cd apps/web && "$BUN" install --frozen-lockfile >/dev/null && "$BUN" run lint && "$BUN" run typecheck && "$BUN" run test)
fi
echo "check: ok"
