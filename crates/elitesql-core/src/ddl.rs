//! Crash-safe DDL: the pending-operation record and the data transforms.
//!
//! Schema changes that also touch data (renaming a table or a column,
//! dropping a column, backfilling a new column's default) are two-part
//! operations: rewrite the data, then publish the new catalog. Each part is
//! atomic on its own, but a crash in between would leave the two halves
//! disagreeing. So the operation first records what it is about to do in
//! `ddl.json`; `Db::open` replays a pending record to completion before
//! serving anything, and only then clears it. Every step is idempotent, so
//! replaying a step that already ran is a no-op.
//!
//! `DROP TABLE`, `DROP INDEX` and a plain `ADD COLUMN` need none of this:
//! they are a single durable catalog write.

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::manifest::fsync_dir;
use crate::schema::{Catalog, Column};

pub(crate) const DDL_FILE: &str = "ddl.json";
const DDL_TMP_FILE: &str = "ddl.json.tmp";

/// A DDL operation that was started and may not have finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub(crate) enum DdlIntent {
    RenameTable {
        table: String,
        to: String,
    },
    RenameColumn {
        table: String,
        column: String,
        to: String,
    },
    DropColumn {
        table: String,
        column: String,
    },
    AddColumn {
        table: String,
        column: Column,
        /// `NOT NULL` is applied only after every record has been backfilled.
        not_null: bool,
    },
}

impl DdlIntent {
    pub fn load(dir: &Path) -> Result<Option<DdlIntent>> {
        let bytes = match fs::read(dir.join(DDL_FILE)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Corrupt(format!("invalid {DDL_FILE}: {e}")))
    }

    /// Record the intent durably before the first step runs.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).expect("ddl intent always serializes");
        let tmp = dir.join(DDL_TMP_FILE);
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, dir.join(DDL_FILE))?;
        fsync_dir(dir)
    }

    /// Clear the record once every step has completed.
    pub fn clear(dir: &Path) -> Result<()> {
        match fs::remove_file(dir.join(DDL_FILE)) {
            Ok(()) => fsync_dir(dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            DdlIntent::RenameTable { table, to } => format!("ALTER TABLE {table} RENAME TO {to}"),
            DdlIntent::RenameColumn { table, column, to } => {
                format!("ALTER TABLE {table} RENAME COLUMN {column} TO {to}")
            }
            DdlIntent::DropColumn { table, column } => {
                format!("ALTER TABLE {table} DROP COLUMN {column}")
            }
            DdlIntent::AddColumn { table, column, .. } => {
                format!("ALTER TABLE {table} ADD COLUMN {}", column.name)
            }
        }
    }
}

/// A transform applied while every segment is rewritten. `None` is a plain
/// compaction; the others carry a DDL change through both the data and the
/// catalog in one pass.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Rewrite<'a> {
    None,
    RenameTable {
        from: &'a str,
        to: &'a str,
    },
    RenameColumn {
        table: &'a str,
        from: &'a str,
        to: &'a str,
    },
    DropColumn {
        table: &'a str,
        column: &'a str,
    },
}

impl Rewrite<'_> {
    pub fn is_ddl(&self) -> bool {
        !matches!(self, Rewrite::None)
    }

    /// The name a table's entries are written under after the transform.
    pub fn output_table(&self, table: &str) -> String {
        match *self {
            Rewrite::RenameTable { from, to } if from == table => to.to_owned(),
            _ => table.to_owned(),
        }
    }

    /// Apply the catalog half of the transform. Idempotent: re-applying a
    /// change that is already present does nothing, which is what makes crash
    /// recovery safe.
    pub fn apply_to_catalog(&self, catalog: &mut Catalog) {
        match *self {
            Rewrite::None => {}
            Rewrite::RenameTable { from, to } => {
                if let Some(t) = catalog.table_mut(from) {
                    t.name = to.to_owned();
                }
            }
            Rewrite::RenameColumn { table, from, to } => {
                let Some(t) = catalog.table_mut(table) else {
                    return;
                };
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == from) {
                    c.name = to.to_owned();
                }
                for d in t.indexes.iter_mut().filter(|d| d.column == from) {
                    d.column = to.to_owned();
                }
                for d in t.vector_indexes.iter_mut().filter(|d| d.column == from) {
                    d.column = to.to_owned();
                }
                for d in t.text_indexes.iter_mut().filter(|d| d.column == from) {
                    d.column = to.to_owned();
                }
            }
            Rewrite::DropColumn { table, column } => {
                let Some(t) = catalog.table_mut(table) else {
                    return;
                };
                t.columns.retain(|c| c.name != column);
                t.indexes.retain(|d| d.column != column);
                t.vector_indexes.retain(|d| d.column != column);
                t.text_indexes.retain(|d| d.column != column);
            }
        }
    }
}
