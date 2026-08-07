use std::fmt;

/// Errors surfaced by the engine. The set will grow into the full
/// `clawdb_status` catalog during Phase 1.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// On-disk data failed validation (bad checksum, bad framing, bad marker).
    Corrupt(String),
    TableExists(String),
    TableNotFound(String),
    RecordNotFound { table: String, id: String },
    DuplicateId { table: String, id: String },
    /// The record or patch does not conform to the table schema.
    SchemaViolation(String),
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, Error>;

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
