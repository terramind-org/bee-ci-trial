//! In-memory forge store: faithful to the [`ForgeStore`] contract, for tests and dev mode.
//!
//! Each surface has its own lock; a transaction holds its surface's lock from `begin` to
//! commit or drop, so transactions on one surface are serializable and transactions on
//! different surfaces never wait for each other. Writes are applied in place with an undo
//! log; dropping an uncommitted transaction replays the undo log.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, RwLock};
use tokio::time::Instant;

use super::{
    ForgeStore, ForgeTx, Key, Migration, MigrationReport, OutboxRecord, PublicationLock, Row, Scan,
    Surface, TableDef, check_surface, migrate, within,
};
use crate::error::{ForgeError, Result};

type RowKey = (String, String);

#[derive(Default)]
struct SurfaceData {
    tables: HashMap<&'static str, BTreeMap<RowKey, Row>>,
    outbox: BTreeMap<String, Vec<u8>>,
}

enum Undo {
    Row {
        table: &'static str,
        key: RowKey,
        prev: Option<Row>,
    },
    Outbox {
        id: String,
        prev: Option<Vec<u8>>,
    },
}

/// The in-memory store.
#[derive(Default)]
pub struct MemoryStore {
    surfaces: Mutex<HashMap<&'static str, Arc<AsyncMutex<SurfaceData>>>>,
    applied: Mutex<Vec<(String, String)>>,
    publication_locks: Mutex<HashMap<String, Arc<RwLock<()>>>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }

    fn surface(&self, s: Surface) -> Result<Arc<AsyncMutex<SurfaceData>>> {
        let mut map = self
            .surfaces
            .lock()
            .map_err(|_| ForgeError::internal("memory store poisoned"))?;
        Ok(map.entry(s.0).or_default().clone())
    }
}

#[async_trait]
impl ForgeStore for MemoryStore {
    fn backend(&self) -> &'static str {
        "memory"
    }

    async fn publication_lock(
        &self,
        org: &str,
        shared: bool,
        deadline: Instant,
    ) -> Result<PublicationLock> {
        let lock = self
            .publication_locks
            .lock()
            .map_err(|_| ForgeError::internal("memory store poisoned"))?
            .entry(org.to_string())
            .or_default()
            .clone();
        within(deadline, async move {
            let permit: PublicationLock = if shared {
                Box::new(lock.read_owned().await)
            } else {
                Box::new(lock.write_owned().await)
            };
            Ok(permit)
        })
        .await
    }

    async fn migrate(&self, migrations: &[&Migration]) -> Result<MigrationReport> {
        migrate::validate(migrations)?;
        let mut applied = self
            .applied
            .lock()
            .map_err(|_| ForgeError::internal("memory store poisoned"))?;
        let mut report = MigrationReport::default();
        for m in migrations {
            for s in m.surfaces() {
                let id = format!("{}@{}", m.id(), s);
                let sum = m.checksum();
                match applied.iter().find(|(a, _)| *a == id) {
                    Some((_, prev)) if *prev != sum => {
                        return Err(ForgeError::internal(format!(
                            "migration {id} was applied with a different checksum; applied \
                             migrations are immutable"
                        )));
                    }
                    Some(_) => report.already_applied.push(id),
                    None => {
                        applied.push((id.clone(), sum));
                        report.applied.push(id);
                    }
                }
            }
        }
        Ok(report)
    }

    async fn begin(&self, surface: Surface, deadline: Instant) -> Result<Box<dyn ForgeTx>> {
        let lock = self.surface(surface)?;
        let guard = tokio::time::timeout_at(deadline, lock.lock_owned())
            .await
            .map_err(|_| ForgeError::deadline())?;
        Ok(Box::new(MemoryTx {
            surface,
            data: guard,
            undo: Vec::new(),
            done: false,
        }))
    }
}

struct MemoryTx {
    surface: Surface,
    data: OwnedMutexGuard<SurfaceData>,
    undo: Vec<Undo>,
    done: bool,
}

impl MemoryTx {
    fn table(&mut self, t: &TableDef) -> Result<&mut BTreeMap<RowKey, Row>> {
        check_surface(self.surface, t)?;
        Ok(self.data.tables.entry(t.name).or_default())
    }

    fn unique_ok(&mut self, t: &TableDef, row: &Row) -> Result<()> {
        let table = self.table(t)?;
        for (key, other) in table.iter() {
            if key.0 != row.scope || key.1 == row.id {
                continue;
            }
            let clash = (t.unique_k1 && row.k1.is_some() && other.k1 == row.k1)
                || (t.unique_k2 && row.k2.is_some() && other.k2 == row.k2);
            if clash {
                return Err(ForgeError::already_exists(format!(
                    "{} key in {}",
                    t.name, row.scope
                )));
            }
        }
        Ok(())
    }

    fn write(&mut self, t: &TableDef, row: Row) -> Result<()> {
        let key = (row.scope.clone(), row.id.clone());
        let table = self.table(t)?;
        let prev = table.insert(key.clone(), row);
        self.undo.push(Undo::Row {
            table: t.name,
            key,
            prev,
        });
        Ok(())
    }
}

impl Drop for MemoryTx {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        while let Some(u) = self.undo.pop() {
            match u {
                Undo::Row { table, key, prev } => {
                    let t = self.data.tables.entry(table).or_default();
                    match prev {
                        Some(r) => {
                            t.insert(key, r);
                        }
                        None => {
                            t.remove(&key);
                        }
                    }
                }
                Undo::Outbox { id, prev } => match prev {
                    Some(b) => {
                        self.data.outbox.insert(id, b);
                    }
                    None => {
                        self.data.outbox.remove(&id);
                    }
                },
            }
        }
    }
}

#[async_trait]
impl ForgeTx for MemoryTx {
    fn surface(&self) -> Surface {
        self.surface
    }

    async fn get(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>> {
        let table = self.table(t)?;
        Ok(table.get(&(scope.to_string(), id.to_string())).cloned())
    }

    async fn get_for_update(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>> {
        self.get(t, scope, id).await
    }

    async fn insert(&mut self, t: &TableDef, row: &Row) -> Result<()> {
        let key = (row.scope.clone(), row.id.clone());
        if self.table(t)?.contains_key(&key) {
            return Err(ForgeError::already_exists(format!(
                "{} row {}",
                t.name, row.id
            )));
        }
        self.unique_ok(t, row)?;
        let mut r = row.clone();
        r.version = 1;
        self.write(t, r)
    }

    async fn update(&mut self, t: &TableDef, row: &Row) -> Result<u64> {
        let key = (row.scope.clone(), row.id.clone());
        let current = match self.table(t)?.get(&key) {
            Some(c) => c.version,
            None => return Err(ForgeError::not_found(format!("{} row {}", t.name, row.id))),
        };
        if current != row.version {
            return Err(ForgeError::version_mismatch(row.version, current));
        }
        self.unique_ok(t, row)?;
        let mut r = row.clone();
        r.version = current + 1;
        self.write(t, r)?;
        Ok(current + 1)
    }

    async fn put(&mut self, t: &TableDef, row: &Row) -> Result<u64> {
        let key = (row.scope.clone(), row.id.clone());
        let next = self.table(t)?.get(&key).map(|c| c.version + 1).unwrap_or(1);
        self.unique_ok(t, row)?;
        let mut r = row.clone();
        r.version = next;
        self.write(t, r)?;
        Ok(next)
    }

    async fn delete(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<bool> {
        let key = (scope.to_string(), id.to_string());
        let prev = self.table(t)?.remove(&key);
        let existed = prev.is_some();
        if existed {
            self.undo.push(Undo::Row {
                table: t.name,
                key,
                prev,
            });
        }
        Ok(existed)
    }

    async fn scan(&mut self, t: &TableDef, q: &Scan) -> Result<Vec<Row>> {
        let table = self.table(t)?;
        let mut rows: Vec<&Row> = table
            .values()
            .filter(|r| q.matches(r) && q.past_cursor(r))
            .collect();
        rows.sort_by_key(|r| q.sort_key(r));
        if q.descending {
            rows.reverse();
        }
        Ok(rows
            .into_iter()
            .take(q.effective_limit())
            .cloned()
            .collect())
    }

    async fn count(&mut self, t: &TableDef, scope: &str, key: Option<&Key>) -> Result<u64> {
        let q = Scan {
            scope: Some(scope.to_string()),
            key: key.cloned(),
            ..Scan::default()
        };
        let table = self.table(t)?;
        Ok(table.values().filter(|r| q.matches(r)).count() as u64)
    }

    async fn add_n1(&mut self, t: &TableDef, scope: &str, id: &str, delta: i64) -> Result<i64> {
        let key = (scope.to_string(), id.to_string());
        let mut row = self
            .table(t)?
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Row::new(scope, id));
        let value = row.n1.unwrap_or(0) + delta;
        row.n1 = Some(value);
        row.version += 1;
        self.write(t, row)?;
        Ok(value)
    }

    async fn outbox_push(&mut self, id: &str, record: &OutboxRecord) -> Result<()> {
        let prev = self
            .data
            .outbox
            .insert(id.to_string(), record.encode_to_vec());
        self.undo.push(Undo::Outbox {
            id: id.to_string(),
            prev,
        });
        Ok(())
    }

    async fn outbox_peek(&mut self, limit: usize) -> Result<Vec<(String, OutboxRecord)>> {
        self.data
            .outbox
            .iter()
            .take(limit)
            .map(|(id, b)| Ok((id.clone(), OutboxRecord::decode(b.as_slice())?)))
            .collect()
    }

    async fn outbox_remove(&mut self, ids: &[String]) -> Result<()> {
        for id in ids {
            if let Some(prev) = self.data.outbox.remove(id) {
                self.undo.push(Undo::Outbox {
                    id: id.clone(),
                    prev: Some(prev),
                });
            }
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        self.done = true;
        self.undo.clear();
        Ok(())
    }
}
