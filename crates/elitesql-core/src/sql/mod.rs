//! The EliteSQL SQL dialect: a deliberately small V1 subset.
//!
//! Supported: CREATE TABLE, CREATE [UNIQUE] INDEX, INSERT (column list +
//! VALUES), SELECT with WHERE / INNER / LEFT / RIGHT JOIN / ORDER BY /
//! LIMIT / OFFSET, UPDATE ... SET literal, DELETE. Everything else is
//! rejected at parse time with an explicit "not supported in V1" error.
//!
//! Decision log: the parser is hand-written recursive descent rather than a
//! restricted `sqlparser-rs`. The subset is small and closed, error messages
//! stay under our control, no new dependency enters the core, and the parser
//! is bounded (expression depth guard) so fuzzing cannot overflow the stack.

mod ast;
mod exec;
mod lexer;
mod parser;

pub(crate) use exec::{
    execute, execute_cursor, execute_cursor_named, execute_cursor_positional, execute_named,
    execute_positional,
};
pub use exec::{QueryCursor, QueryOutput};
