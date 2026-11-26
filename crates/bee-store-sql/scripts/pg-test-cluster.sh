#!/usr/bin/env bash
# Throwaway Postgres for bee-store-sql's gated tests: a primary and a streaming standby in a
# temp directory, on free ports, deleted afterwards.
#
#   scripts/pg-test-cluster.sh run [cmd...]   start, run cmd (default: cargo test -p bee-store-sql)
#                                             with BEE_TEST_PG / BEE_TEST_PG_REPLICA set,
#                                             then stop and delete everything
#
# Needs initdb/pg_ctl/pg_basebackup on PATH or under `brew --prefix postgresql@17`.
set -euo pipefail

bin=""
if ! command -v initdb >/dev/null 2>&1; then
  prefix="$(brew --prefix postgresql@17 2>/dev/null || true)"
  [ -n "$prefix" ] && bin="$prefix/bin/"
fi

free_port() {
  local p
  for p in $(seq "$1" $(($1 + 200))); do
    if ! (echo >/dev/tcp/127.0.0.1/"$p") 2>/dev/null; then echo "$p"; return; fi
  done
  echo "no free port near $1" >&2; exit 1
}

cmd="${1:-run}"; shift || true
[ "$cmd" = run ] || { echo "usage: $0 run [cmd...]" >&2; exit 2; }

dir="$(mktemp -d "${TMPDIR:-/tmp}/bee-pg.XXXXXX")"
primary_port="$(free_port 55432)"
cleanup() {
  "${bin}pg_ctl" -D "$dir/standby" -m immediate stop >/dev/null 2>&1 || true
  "${bin}pg_ctl" -D "$dir/primary" -m immediate stop >/dev/null 2>&1 || true
  rm -rf "$dir"
}
trap cleanup EXIT

"${bin}initdb" -D "$dir/primary" -U postgres -A trust --no-sync >/dev/null
cat >>"$dir/primary/postgresql.conf" <<EOF
port = $primary_port
listen_addresses = '127.0.0.1'
unix_socket_directories = '$dir'
max_connections = 200
shared_buffers = 64MB
wal_level = replica
max_wal_senders = 4
fsync = on
synchronous_commit = on
EOF
echo "host replication postgres 127.0.0.1/32 trust" >>"$dir/primary/pg_hba.conf"
"${bin}pg_ctl" -D "$dir/primary" -l "$dir/primary.log" -w start >/dev/null

standby_port="$(free_port $((primary_port + 1)))"
"${bin}pg_basebackup" -h 127.0.0.1 -p "$primary_port" -U postgres -D "$dir/standby" -R -X stream >/dev/null
cat >>"$dir/standby/postgresql.conf" <<EOF
port = $standby_port
hot_standby = on
EOF
"${bin}pg_ctl" -D "$dir/standby" -l "$dir/standby.log" -w start >/dev/null

export BEE_TEST_PG="postgres://postgres@127.0.0.1:$primary_port/postgres"
export BEE_TEST_PG_REPLICA="postgres://postgres@127.0.0.1:$standby_port/postgres"
echo "primary $BEE_TEST_PG, standby $BEE_TEST_PG_REPLICA" >&2
if [ "$#" -eq 0 ]; then
  set -- cargo test -p bee-store-sql
fi
"$@"
