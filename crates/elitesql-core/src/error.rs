use std::fmt;

/// Errors surfaced by the engine. `code()` gives the stable numeric status
/// that the C ABI will expose as `elitesql_status` in Phase 4.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// On-disk data failed validation (bad checksum, bad framing, bad marker).
    Corrupt(String),
    TableExists(String),
    TableNotFound(String),
    RecordNotFound {
        table: String,
        id: String,
    },
    DuplicateId {
        table: String,
        id: String,
    },
    /// The record or patch does not conform to the table schema.
    SchemaViolation(String),
    InvalidArgument(String),
    /// Optimistic commit validation failed: something this transaction wrote
    /// was modified by a commit published after the transaction began.
    /// The caller should retry the transaction (CONFLICT_RETRY).
    Conflict(String),
    /// Another process holds the database lock.
    DatabaseLocked(String),
    /// A unique index rejected a duplicate value at commit.
    UniqueViolation {
        table: String,
        column: String,
    },
    /// SQL parse or execution error, including features outside the V1 subset.
    Sql(String),
    /// The database was opened read-only; writes are rejected.
    ReadOnly,
    /// The table exists but has no such column (DDL targeting a column).
    ColumnNotFound {
        table: String,
        column: String,
    },
    /// No index of the requested kind exists on that column.
    IndexNotFound {
        table: String,
        column: String,
    },
    /// One operation cannot fit in its configured database-wide memory pool.
    /// Concurrent operations wait for permits; intrinsically oversized work
    /// must use batching/streaming or a larger configured pool.
    MemoryLimit(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Stable status code for the future C ABI. 0 is reserved for OK.
    pub fn code(&self) -> u32 {
        match self {
            Error::Io(_) => 1,
            Error::Corrupt(_) => 2,
            Error::TableExists(_) => 3,
            Error::TableNotFound(_) => 4,
            Error::RecordNotFound { .. } => 5,
            Error::DuplicateId { .. } => 6,
            Error::SchemaViolation(_) => 7,
            Error::InvalidArgument(_) => 8,
            Error::Conflict(_) => 9,
            Error::DatabaseLocked(_) => 10,
            Error::UniqueViolation { .. } => 11,
            Error::Sql(_) => 12,
            Error::ReadOnly => 13,
            Error::ColumnNotFound { .. } => 14,
            Error::IndexNotFound { .. } => 15,
            Error::MemoryLimit(_) => 16,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corrupt(msg) => write!(f, "corrupt database: {msg}"),
            Error::TableExists(name) => write!(f, "table already exists: {name}"),
            Error::TableNotFound(name) => write!(f, "table not found: {name}"),
            Error::RecordNotFound { table, id } => {
                write!(f, "record not found: {table}/{id}")
            }
            Error::DuplicateId { table, id } => {
                write!(f, "duplicate id: {table}/{id}")
            }
            Error::SchemaViolation(msg) => write!(f, "schema violation: {msg}"),
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Error::Conflict(msg) => write!(f, "conflict, retry transaction: {msg}"),
            Error::DatabaseLocked(path) => {
                write!(f, "database is locked by another process: {path}")
            }
            Error::UniqueViolation { table, column } => {
                write!(f, "unique index violation on {table}.{column}")
            }
            Error::Sql(msg) => write!(f, "sql error: {msg}"),
            Error::ReadOnly => write!(f, "database opened read-only; writes are rejected"),
            Error::ColumnNotFound { table, column } => {
                write!(f, "column not found: {table}.{column}")
            }
            Error::IndexNotFound { table, column } => {
                write!(f, "index not found on {table}.{column}")
            }
            Error::MemoryLimit(msg) => write!(f, "memory limit exceeded: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
