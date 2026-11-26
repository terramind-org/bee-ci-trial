# bee-dbbench

Measures candidate metadata databases on the three workloads that decide bee's storage
design. One binary, four backends, one JSON line per run.

| Workload | What it does | Why it matters |
|---|---|---|
| `h1` hot ref | N concurrent "landers" each land a change on the same ref: lock the ref row, bump its version, append a ref-log row and a landed-index row, commit. `naive` runs one transaction per land. `coalesced` sends every land to one owner task that commits everything queued as one transaction. | Every agent lands on `main`. This is the single hottest row in the system. |
| `h2` ingest | W writers insert-or-ignore content-addressed object rows in batches of 1,000, with 30% of each batch repeating ids already sent. | Every commit writes object, edge and index rows; duplicates are normal because content is deduplicated. |
| `h3` point reads | R readers fetch random object rows by id. | Most metadata reads are point reads of immutable rows. |

## Backends and settings used

- **Postgres 17.11** (Homebrew), single node: `synchronous_commit = on`, `fsync = on`,
  `wal_sync_method = fsync_writethrough`, `shared_buffers = 2GB`, `max_connections = 1100`.
- **YugabyteDB 2025.2.0.0** (official macOS arm64 build), one node, replication factor 1,
  `durable_wal_write = true`, YSQL. The `objects` table is hash-partitioned on `(org, id)`.
- **SQLite** (bundled with rusqlite 0.40), WAL, `synchronous = FULL`, `fullfsync = ON`.
- **FoundationDB 7.3.77** (official macOS arm64 package), one process, `single` redundancy,
  `ssd-redwood-1` storage engine.

### Durability is equalized

On macOS a plain `fsync()` returns once data reaches the drive, without flushing the drive's
cache. Only `fcntl(F_FULLFSYNC)` does. On the machine below, our recorded run of
`fsync_probe.py` gave a median of 0.051 ms for `fsync` and 4.624 ms for `F_FULLFSYNC`. FoundationDB issues `F_FULLFSYNC` by default; Postgres and SQLite do
not. So every run here has Postgres and SQLite configured to flush fully as well. YugabyteDB is
configured to sync its WAL on every write; whether that sync is a full flush on macOS we could
not confirm, and its single-lander commit latency (below the measured flush time) suggests it is
not. Read its hot-ref numbers with that in mind.

## Limits

- **One laptop.** Apple M4 Pro, 14 cores, 24 GB, internal SSD, macOS. The database and the load
  generator share the machine. No network, no replicas, no second zone.
- **Single-node YugabyteDB and FoundationDB.** A production deployment would replicate across
  three zones; that adds a consensus round trip to every commit, which these runs do not include.
- **Small data.** Ingest stops at 5 million offered rows (about 3.5 million unique), all of which
  fit in memory. Behaviour at a billion rows is not measured here.
- **Naive SQL landers cap at 256.** Naive mode opens one connection per lander; 1,000 Postgres or
  YugabyteDB backend processes would exhaust this machine's memory, so those cells are not run.
  Coalesced mode uses one connection, so it runs at every level.
- Throughput is computed over a fixed 10-second window after all connections are open.

## Run it

```sh
# 1. Start the databases (see the settings above), then:
export FDB_LIB=/path/to/dir/with/libfdb_c.dylib
./run.sh                      # writes results/<UTC stamp>/results.jsonl and env.txt
python3 summarize.py results/<run>/results.jsonl
```

Environment variables override the defaults: `DBS`, `LANDERS`, `H1_SECS`, `ROWS`, `WRITERS`,
`READERS`, `PG_URL`, `YB_URL`, `SQLITE_PATH`, `FDB_CLUSTER`.

`results/discarded-*` hold runs we threw away: a first attempt whose throughput included
connection setup, and a run that accidentally overlapped another. They are kept for the record,
not used.
