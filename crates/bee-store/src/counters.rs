//! Storage access counters for complexity gates (SPEC §14.6).
//!
//! A store counts one read per row it reads and one write per row it writes, per table, plus
//! transactions, object fetches by kind and blob-store calls. Tests take a [`AccessSnapshot`]
//! before and after an operation and assert on the difference.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bee_core::ObjectKind;

/// A metadata table (see [`crate::types`] for the schema).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Table {
    /// `orgs`.
    Orgs,
    /// `repos`.
    Repos,
    /// `objects`.
    Objects,
    /// `object_uploads`.
    Uploads,
    /// `changesets`.
    Changesets,
    /// `parent_edges`.
    ParentEdges,
    /// `object_paths`.
    ObjectPaths,
    /// `derived`.
    Derived,
    /// `refs`.
    Refs,
    /// `ref_log`.
    RefLog,
    /// `landed`.
    Landed,
    /// `event_counters`.
    EventCounters,
    /// `events`.
    Events,
    /// `redactions`.
    Redactions,
    /// `redaction_epochs`.
    RedactionEpochs,
    /// `gc_marks`.
    GcMarks,
}

impl Table {
    /// Every table, in declaration order.
    pub const ALL: [Table; 16] = [
        Table::Orgs,
        Table::Repos,
        Table::Objects,
        Table::Uploads,
        Table::Changesets,
        Table::ParentEdges,
        Table::ObjectPaths,
        Table::Derived,
        Table::Refs,
        Table::RefLog,
        Table::Landed,
        Table::EventCounters,
        Table::Events,
        Table::Redactions,
        Table::RedactionEpochs,
        Table::GcMarks,
    ];

    /// The SQL table name.
    pub fn as_str(self) -> &'static str {
        match self {
            Table::Orgs => "orgs",
            Table::Repos => "repos",
            Table::Objects => "objects",
            Table::Uploads => "object_uploads",
            Table::Changesets => "changesets",
            Table::ParentEdges => "parent_edges",
            Table::ObjectPaths => "object_paths",
            Table::Derived => "derived",
            Table::Refs => "refs",
            Table::RefLog => "ref_log",
            Table::Landed => "landed",
            Table::EventCounters => "event_counters",
            Table::Events => "events",
            Table::Redactions => "redactions",
            Table::RedactionEpochs => "redaction_epochs",
            Table::GcMarks => "gc_marks",
        }
    }

    fn index(self) -> usize {
        Table::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Object kinds counted separately for fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FetchKind {
    /// Blobs.
    Blob,
    /// Trees.
    Tree,
    /// Metas.
    Meta,
    /// Anything else.
    Other,
}

impl FetchKind {
    /// The bucket for an object kind.
    pub fn of(kind: ObjectKind) -> FetchKind {
        match kind {
            ObjectKind::Blob => FetchKind::Blob,
            ObjectKind::Tree => FetchKind::Tree,
            ObjectKind::Meta => FetchKind::Meta,
            _ => FetchKind::Other,
        }
    }

    fn index(self) -> usize {
        match self {
            FetchKind::Blob => 0,
            FetchKind::Tree => 1,
            FetchKind::Meta => 2,
            FetchKind::Other => 3,
        }
    }

    const ALL: [FetchKind; 4] = [
        FetchKind::Blob,
        FetchKind::Tree,
        FetchKind::Meta,
        FetchKind::Other,
    ];
}

/// Blob-store call counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlobAccess {
    /// `put` calls.
    pub puts: u64,
    /// `get` and `get_range` calls that returned bytes.
    pub gets: u64,
    /// `get_range` calls.
    pub range_gets: u64,
    /// Ids checked by `has`.
    pub has: u64,
    /// Deletes that removed bytes.
    pub deletes: u64,
    /// Bytes returned by reads.
    pub bytes_read: u64,
    /// Reads served from the cold class.
    pub cold_reads: u64,
}

/// Live counters, shared by a meta store and a blob store.
#[derive(Debug, Default)]
pub struct AccessCounters {
    reads: [AtomicU64; 16],
    writes: [AtomicU64; 16],
    fetches: [AtomicU64; 4],
    transactions: AtomicU64,
    write_transactions: AtomicU64,
    replica_reads: AtomicU64,
    replica_forwards: AtomicU64,
    changeset_bytes: AtomicU64,
    blob_puts: AtomicU64,
    blob_gets: AtomicU64,
    blob_range_gets: AtomicU64,
    blob_has: AtomicU64,
    blob_deletes: AtomicU64,
    blob_bytes_read: AtomicU64,
    blob_cold_reads: AtomicU64,
}

fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

impl AccessCounters {
    /// Fresh zeroed counters.
    pub fn new() -> AccessCounters {
        AccessCounters::default()
    }

    /// Counts `n` row reads of `table`.
    pub fn read(&self, table: Table, n: u64) {
        add(&self.reads[table.index()], n);
    }

    /// Counts `n` row writes of `table`.
    pub fn write(&self, table: Table, n: u64) {
        add(&self.writes[table.index()], n);
    }

    /// Counts one object fetch (a row or bytes read for its content).
    pub fn fetch(&self, kind: ObjectKind) {
        add(&self.fetches[FetchKind::of(kind).index()], 1);
    }

    /// Counts one transaction.
    pub fn transaction(&self) {
        add(&self.transactions, 1);
    }

    /// Counts one read-write transaction (a durable commit); also counted by
    /// [`AccessCounters::transaction`]. The SQL stores count these.
    pub fn write_transaction(&self) {
        add(&self.write_transactions, 1);
    }

    /// Counts one read served by a replica.
    pub fn replica_read(&self) {
        add(&self.replica_reads, 1);
    }

    /// Counts one replica read forwarded to the primary because of `min_version`.
    pub fn replica_forward(&self) {
        add(&self.replica_forwards, 1);
    }

    /// Counts `n` bytes of changeset rows read.
    pub fn changeset_bytes(&self, n: u64) {
        add(&self.changeset_bytes, n);
    }

    /// Counts a blob-store put.
    pub fn blob_put(&self) {
        add(&self.blob_puts, 1);
    }

    /// Counts a blob-store read of `bytes` bytes; `cold` when served from the cold class.
    pub fn blob_get(&self, kind: ObjectKind, bytes: u64, range: bool, cold: bool) {
        add(&self.blob_gets, 1);
        add(&self.blob_bytes_read, bytes);
        if range {
            add(&self.blob_range_gets, 1);
        }
        if cold {
            add(&self.blob_cold_reads, 1);
        }
        self.fetch(kind);
    }

    /// Counts ids checked by `has`.
    pub fn blob_has(&self, n: u64) {
        add(&self.blob_has, n);
    }

    /// Counts a blob delete.
    pub fn blob_delete(&self) {
        add(&self.blob_deletes, 1);
    }

    /// A consistent-enough copy of every counter.
    pub fn snapshot(&self) -> AccessSnapshot {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut reads = BTreeMap::new();
        let mut writes = BTreeMap::new();
        for t in Table::ALL {
            reads.insert(t, load(&self.reads[t.index()]));
            writes.insert(t, load(&self.writes[t.index()]));
        }
        let fetches = FetchKind::ALL
            .iter()
            .map(|k| (*k, load(&self.fetches[k.index()])))
            .collect();
        AccessSnapshot {
            reads,
            writes,
            fetches,
            transactions: load(&self.transactions),
            write_transactions: load(&self.write_transactions),
            replica_reads: load(&self.replica_reads),
            replica_forwards: load(&self.replica_forwards),
            changeset_bytes_read: load(&self.changeset_bytes),
            blob: BlobAccess {
                puts: load(&self.blob_puts),
                gets: load(&self.blob_gets),
                range_gets: load(&self.blob_range_gets),
                has: load(&self.blob_has),
                deletes: load(&self.blob_deletes),
                bytes_read: load(&self.blob_bytes_read),
                cold_reads: load(&self.blob_cold_reads),
            },
        }
    }
}

/// A copy of the counters at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessSnapshot {
    /// Row reads per table.
    pub reads: BTreeMap<Table, u64>,
    /// Row writes per table.
    pub writes: BTreeMap<Table, u64>,
    /// Object fetches per kind (object rows read for content plus blob-store reads).
    pub fetches: BTreeMap<FetchKind, u64>,
    /// Transactions.
    pub transactions: u64,
    /// Read-write transactions among them (durable commits; counted by the SQL stores).
    pub write_transactions: u64,
    /// Reads served by a replica.
    pub replica_reads: u64,
    /// Replica reads forwarded to the primary.
    pub replica_forwards: u64,
    /// Bytes of changeset rows read (the canonical encoding, or the prefix of it read for
    /// a root): grows with the deltas read, which row counts do not show.
    pub changeset_bytes_read: u64,
    /// Blob-store calls.
    pub blob: BlobAccess,
}

impl AccessSnapshot {
    /// Row reads of one table.
    pub fn reads_of(&self, table: Table) -> u64 {
        self.reads.get(&table).copied().unwrap_or(0)
    }

    /// Row writes of one table.
    pub fn writes_of(&self, table: Table) -> u64 {
        self.writes.get(&table).copied().unwrap_or(0)
    }

    /// Fetches of one kind.
    pub fn fetches_of(&self, kind: FetchKind) -> u64 {
        self.fetches.get(&kind).copied().unwrap_or(0)
    }

    /// `self - earlier`, counter by counter (saturating).
    pub fn since(&self, earlier: &AccessSnapshot) -> AccessSnapshot {
        let sub = |a: &BTreeMap<Table, u64>, b: &BTreeMap<Table, u64>| {
            a.iter()
                .map(|(k, v)| (*k, v.saturating_sub(b.get(k).copied().unwrap_or(0))))
                .collect()
        };
        AccessSnapshot {
            reads: sub(&self.reads, &earlier.reads),
            writes: sub(&self.writes, &earlier.writes),
            fetches: self
                .fetches
                .iter()
                .map(|(k, v)| (*k, v.saturating_sub(earlier.fetches_of(*k))))
                .collect(),
            transactions: self.transactions.saturating_sub(earlier.transactions),
            write_transactions: self
                .write_transactions
                .saturating_sub(earlier.write_transactions),
            replica_reads: self.replica_reads.saturating_sub(earlier.replica_reads),
            replica_forwards: self
                .replica_forwards
                .saturating_sub(earlier.replica_forwards),
            changeset_bytes_read: self
                .changeset_bytes_read
                .saturating_sub(earlier.changeset_bytes_read),
            blob: BlobAccess {
                puts: self.blob.puts.saturating_sub(earlier.blob.puts),
                gets: self.blob.gets.saturating_sub(earlier.blob.gets),
                range_gets: self.blob.range_gets.saturating_sub(earlier.blob.range_gets),
                has: self.blob.has.saturating_sub(earlier.blob.has),
                deletes: self.blob.deletes.saturating_sub(earlier.blob.deletes),
                bytes_read: self.blob.bytes_read.saturating_sub(earlier.blob.bytes_read),
                cold_reads: self.blob.cold_reads.saturating_sub(earlier.blob.cold_reads),
            },
        }
    }

    /// Total row reads across tables.
    pub fn total_reads(&self) -> u64 {
        self.reads.values().sum()
    }

    /// Total row writes across tables.
    pub fn total_writes(&self) -> u64 {
        self.writes.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_differences() {
        let c = AccessCounters::new();
        let before = c.snapshot();
        c.read(Table::Refs, 2);
        c.write(Table::RefLog, 1);
        c.transaction();
        c.fetch(ObjectKind::Tree);
        c.blob_get(ObjectKind::Blob, 10, true, true);
        c.blob_put();
        c.blob_has(3);
        c.blob_delete();
        c.replica_read();
        c.replica_forward();
        c.changeset_bytes(40);
        let d = c.snapshot().since(&before);
        assert_eq!(d.reads_of(Table::Refs), 2);
        assert_eq!(d.writes_of(Table::RefLog), 1);
        assert_eq!(d.total_reads(), 2);
        assert_eq!(d.total_writes(), 1);
        assert_eq!(d.transactions, 1);
        assert_eq!(d.fetches_of(FetchKind::Tree), 1);
        assert_eq!(d.fetches_of(FetchKind::Blob), 1);
        assert_eq!(d.blob.gets, 1);
        assert_eq!(d.blob.range_gets, 1);
        assert_eq!(d.blob.cold_reads, 1);
        assert_eq!(d.blob.bytes_read, 10);
        assert_eq!(d.blob.has, 3);
        assert_eq!((d.blob.puts, d.blob.deletes), (1, 1));
        assert_eq!((d.replica_reads, d.replica_forwards), (1, 1));
        assert_eq!(d.changeset_bytes_read, 40);
        let names: std::collections::BTreeSet<_> = Table::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(names.len(), Table::ALL.len());
    }
}
