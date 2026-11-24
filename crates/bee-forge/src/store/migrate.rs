//! Embedded, versioned, expand/contract migrations (SPEC §15.3).
//!
//! Each module ships its migrations as data: `<module>/<NNNN>_<name>`, numbered from 1 with
//! no gaps. An applied migration is recorded with a checksum of its operations; a later
//! binary whose migration of the same id has a different checksum refuses to start, so an
//! applied migration is never edited, only followed by a new one.
//!
//! - **Expand** migrations only add (tables, indexes, nullable columns through raw SQL) and
//!   run on every start.
//! - **Contract** migrations remove something. They run only when the store is configured
//!   with `allow_contract` (an operator step after a release in which no code reads the
//!   removed thing); otherwise they are reported as pending and skipped.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::{Index, Surface, TableDef};
use crate::error::{ForgeError, Result};

/// Expand or contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Adds tables, indexes or columns; always safe to run.
    Expand,
    /// Drops something; runs only with `allow_contract`.
    Contract,
}

/// One operation, rendered per dialect by the SQL backends.
#[derive(Debug)]
pub enum MigrationOp {
    /// Creates a table in the standard row shape with its declared indexes.
    CreateTable(&'static TableDef),
    /// Adds an index to an existing table.
    CreateIndex(&'static TableDef, Index),
    /// Raw SQL for one surface, per dialect (each may hold several `;`-separated statements).
    Sql {
        /// The surface whose database it runs in.
        surface: Surface,
        /// SQLite text.
        sqlite: &'static str,
        /// Postgres text.
        postgres: &'static str,
    },
}

impl MigrationOp {
    /// The surface the operation runs in.
    pub fn surface(&self) -> Surface {
        match self {
            MigrationOp::CreateTable(t) | MigrationOp::CreateIndex(t, _) => t.surface,
            MigrationOp::Sql { surface, .. } => *surface,
        }
    }

    fn describe(&self, out: &mut String) {
        match self {
            MigrationOp::CreateTable(t) => {
                let _ = write!(
                    out,
                    "create_table {} {} uk1={} uk2={} idx=",
                    t.name, t.surface, t.unique_k1, t.unique_k2
                );
                for i in t.indexes {
                    let _ = write!(out, "{},", i.suffix());
                }
            }
            MigrationOp::CreateIndex(t, i) => {
                let _ = write!(out, "create_index {} {}", t.name, i.suffix());
            }
            MigrationOp::Sql {
                surface,
                sqlite,
                postgres,
            } => {
                let _ = write!(out, "sql {surface}\n{sqlite}\n{postgres}");
            }
        }
        out.push('\n');
    }
}

/// One migration.
#[derive(Debug)]
pub struct Migration {
    /// Owning module, e.g. `leases`.
    pub module: &'static str,
    /// 1-based number within the module.
    pub version: u32,
    /// snake_case name.
    pub name: &'static str,
    /// Expand or contract.
    pub phase: Phase,
    /// Operations, applied in order in one transaction per surface.
    pub ops: &'static [MigrationOp],
}

impl Migration {
    /// `<module>/<NNNN>_<name>`.
    pub fn id(&self) -> String {
        format!("{}/{:04}_{}", self.module, self.version, self.name)
    }

    /// BLAKE3 of the operations' description, hex.
    pub fn checksum(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "{} {:?}", self.id(), self.phase);
        for op in self.ops {
            op.describe(&mut text);
        }
        blake3::hash(text.as_bytes()).to_hex().to_string()
    }

    /// Surfaces the migration touches.
    pub fn surfaces(&self) -> BTreeSet<Surface> {
        self.ops.iter().map(MigrationOp::surface).collect()
    }
}

/// What a migration run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Newly applied migration ids (per surface: `id@surface`).
    pub applied: Vec<String>,
    /// Already applied before this run.
    pub already_applied: Vec<String>,
    /// Contract migrations skipped because the store does not allow them yet.
    pub pending_contract: Vec<String>,
}

fn snake(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Checks a full migration list: snake_case names, `forge_` table names unique across
/// modules, and per-module numbering 1..=n without gaps.
pub fn validate(migrations: &[&Migration]) -> Result<()> {
    let mut tables = BTreeSet::new();
    let mut modules: Vec<(&str, u32)> = Vec::new();
    for m in migrations {
        if !snake(m.module) || !snake(m.name) {
            return Err(ForgeError::internal(format!(
                "migration {} is not snake_case",
                m.id()
            )));
        }
        let expected = modules
            .iter()
            .rev()
            .find(|(module, _)| *module == m.module)
            .map(|(_, v)| v + 1)
            .unwrap_or(1);
        if m.version != expected {
            return Err(ForgeError::internal(format!(
                "migration {} out of order: expected version {expected}",
                m.id()
            )));
        }
        modules.push((m.module, m.version));
        for op in m.ops {
            if !Surface::valid_name(op.surface().name()) {
                return Err(ForgeError::internal(format!(
                    "bad surface name in {}",
                    m.id()
                )));
            }
            if let MigrationOp::CreateTable(t) = op {
                if !t.name.starts_with("forge_") || !snake(t.name) {
                    return Err(ForgeError::internal(format!(
                        "table {} must be snake_case and start with forge_",
                        t.name
                    )));
                }
                if !tables.insert(t.name) {
                    return Err(ForgeError::internal(format!(
                        "table {} is created twice",
                        t.name
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static T: TableDef = TableDef {
        name: "forge_test_rows",
        surface: Surface("test"),
        unique_k1: false,
        unique_k2: false,
        indexes: &[Index::ScopeK1],
    };
    static M1: Migration = Migration {
        module: "test",
        version: 1,
        name: "create_rows",
        phase: Phase::Expand,
        ops: &[MigrationOp::CreateTable(&T)],
    };
    static M3: Migration = Migration {
        module: "test",
        version: 3,
        name: "skip",
        phase: Phase::Expand,
        ops: &[],
    };

    #[test]
    fn ids_and_checksums_are_stable() {
        assert_eq!(M1.id(), "test/0001_create_rows");
        assert_eq!(M1.checksum(), M1.checksum());
        assert_ne!(M1.checksum(), M3.checksum());
    }

    #[test]
    fn gaps_and_duplicates_are_rejected() {
        assert!(validate(&[&M1]).is_ok());
        assert!(validate(&[&M1, &M3]).is_err());
        assert!(validate(&[&M1, &M1]).is_err());
    }
}
