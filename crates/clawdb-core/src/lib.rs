//! ClawDB core engine — Phase 1: MVP storage.
//!
//! A tiny operational database for AI-native apps. Phase 1 delivers the
//! fail-safe storage core from the spec:
//!
//! - Durable WAL with per-record CRC32 and idempotent replay.
//! - Atomic manifest + `manifest.prev` recovery chain (temp file + rename).
//! - MVCC: transactions read from a stable snapshot; writers stage in
//!   parallel and only meet at commit (optimistic validation, `Conflict`
//!   means retry).
//! - Secondary and unique indexes validated at commit.
//! - Durability modes: `Safe` (fsync per commit), `Balanced`, `Fast`.
//! - Checkpoints drain the WAL into immutable segments; compaction rewrites
//!   segments while preserving versions needed by live snapshots.
//! - Process exclusion via lock file; `check()` for offline validation.
//!
//! After a crash, the database opens to the last fully committed state:
//! a commit is visible completely or not at all.
//!
//! ```
//! use clawdb_core::{Column, ColumnType, Db, Record, TableSchema, Value};
//!
//! let dir = tempfile::tempdir().unwrap();
//! let db = Db::create(dir.path().join("app.clawdb")).unwrap();
//! db.create_table(TableSchema::new(
//!     "docs",
//!     vec![
//!         Column::new("title", ColumnType::Text).not_null(),
//!         Column::new("score", ColumnType::Int64),
//!     ],
//! ))
//! .unwrap();
//!
//! // Auto-commit write, then a multi-op transaction.
//! let id = {
//!     let mut r = Record::new();
//!     r.insert("title".into(), Value::Text("hello".into()));
//!     db.insert("docs", r).unwrap()
//! };
//! let mut txn = db.begin();
//! let mut patch = Record::new();
//! patch.insert("score".into(), Value::Int64(42));
//! txn.update("docs", &id, patch).unwrap();
//! txn.commit().unwrap();
//!
//! let read = db.get("docs", &id).unwrap().unwrap();
//! assert_eq!(read["score"], Value::Int64(42));
//! ```

mod check;
mod db;
mod error;
pub mod jsonio;
mod manifest;
mod repair;
mod schema;
mod segment;
mod sql;
mod value;
mod vector;
mod wal;

pub use check::{check, CheckReport};
pub use repair::{salvage, SalvageReport};
pub use db::{Db, DbOptions, Record, Snapshot, Txn};
pub use error::{Error, Result};
pub use schema::{Column, IndexDef, TableSchema};
pub use sql::QueryOutput;
pub use value::{ColumnType, Value};
pub use vector::{
    IndexingMode, VectorHit, VectorIndexDef, VectorIndexOptions, VectorMetric,
    VectorSearchOptions,
};
pub use wal::Durability;
