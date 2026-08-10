//! Planner and executor for the SQL V1 subset.
//!
//! Heuristic planning, no cost model: point lookup on `id`, secondary index
//! on indexed equality, full scan otherwise. Single-table WHERE conjuncts are
//! pushed below joins. Joins run as index nested-loop whenever the join column
//! on the new side is indexed (or is `id`) and grace hash join otherwise; the
//! choice is static, see `join_uses_index_loop`. RIGHT JOIN is executed by
//! preserving the new table's side, i.e. a LEFT JOIN with roles swapped.
//!
//! Because planning is static and free of estimates, `EXPLAIN <select>`
//! reports the plan the executor will actually run rather than a guess; both
//! read their decisions from `table_driver` and `join_uses_index_loop`.
//!
//! Consistency: SELECT reads the latest committed state (read-committed);
//! snapshot-consistent reads are available through the Rust API (`scan_at`).
//! UPDATE/DELETE run their write set through a transaction whose snapshot is
//! taken BEFORE the row set is read — see the ordering comment in
//! `exec_update` — and retry on optimistic conflict with jittered backoff
//! until `WRITE_RETRY_BUDGET` runs out.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::collate::Collation;
use crate::db::{Db, Record, Snapshot, Txn};
use crate::error::{Error, Result};
use crate::memory::MemoryPermit;
use crate::schema::{TableSchema, ID_COLUMN};
use crate::value::{decode_value, encode_value, ColumnType, Value};
use ulid::Ulid;

use super::ast::*;
use super::parser;

/// Result of `Db::query`.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryOutput {
    /// SELECT result set.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// INSERT: the primary keys of the new records (generated or provided).
    Inserted { ids: Vec<String> },
    /// INSERT into a table with an integer identity. JSON transports retain
    /// the legacy `inserted` field and add identity metadata/lastrowid.
    InsertedIdentity {
        ids: Vec<String>,
        column: String,
        values: Vec<i64>,
    },
    /// UPDATE/DELETE: number of records affected.
    Affected(u64),
    /// DDL statements.
    None,
}

/// Snapshot-consistent, bounded-memory rows for a streaming SELECT.
///
/// The initial implementation accepts the naturally streaming subset: one
/// table, WHERE, projection, OFFSET and LIMIT, without ORDER BY/GROUP BY/JOIN.
/// Blocking operators continue to use [`Db::query`] and its bounded spill
/// machinery until their cursor form is available.
pub struct QueryCursor<'db> {
    _memory: MemoryPermit,
    db: &'db Db,
    snapshot: Snapshot,
    table: String,
    batch_rows: usize,
    after_id: Option<String>,
    batch: std::vec::IntoIter<(String, Record)>,
    predicates: Vec<RExpr>,
    extract: Vec<(usize, String)>,
    columns: Vec<String>,
    offset_remaining: usize,
    limit_remaining: Option<usize>,
    done: bool,
}

impl QueryCursor<'_> {
    /// Output column names, available before consuming the first row.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Pull at most `max_rows` rows. This convenience method preserves the
    /// same bounded behavior as calling `Iterator::next` repeatedly.
    pub fn next_batch(&mut self, max_rows: usize) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::with_capacity(max_rows.min(self.batch_rows));
        while rows.len() < max_rows {
            match self.next() {
                Some(Ok(row)) => rows.push(row),
                Some(Err(error)) => return Err(error),
                None => break,
            }
        }
        Ok(rows)
    }
}

impl Iterator for QueryCursor<'_> {
    type Item = Result<Vec<Value>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.limit_remaining == Some(0) {
            self.done = true;
            return None;
        }
        loop {
            if let Some((_, record)) = self.batch.next() {
                let row = vec![Some(record)];
                match eval_all(&row, &self.predicates) {
                    Err(error) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                    Ok(false) => continue,
                    Ok(true) => {}
                }
                if self.offset_remaining > 0 {
                    self.offset_remaining -= 1;
                    continue;
                }
                if let Some(remaining) = self.limit_remaining.as_mut() {
                    *remaining -= 1;
                }
                return Some(Ok(project_row(&row, &self.extract)));
            }

            let next_batch = match self.db.scan_batch_at_unbudgeted(
                &self.snapshot,
                &self.table,
                self.after_id.as_deref(),
                self.batch_rows,
            ) {
                Ok(batch) => batch,
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            };
            let Some(last_id) = next_batch.last().map(|(id, _)| id.clone()) else {
                self.done = true;
                return None;
            };
            self.after_id = Some(last_id);
            self.batch = next_batch.into_iter();
        }
    }
}

/// How long an autocommit UPDATE/DELETE keeps retrying its optimistic commit
/// before surfacing `Conflict`. The manual promises callers that autocommit
/// writes absorb conflicts themselves; a fixed attempt count broke that
/// promise under contention because colliding writers retried in lockstep.
const WRITE_RETRY_BUDGET: Duration = Duration::from_secs(3);

/// Sleep before the next optimistic retry: exponential from 50µs capped at
/// 6.4ms, with clock-derived jitter so writers that collided once don't
/// collide again in lockstep.
fn backoff_before_retry(attempt: u32) {
    let cap_us = 50u64 << attempt.min(7);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos() as u64)
        .unwrap_or(0);
    std::thread::sleep(Duration::from_micros(1 + nanos % cap_us));
}

/// Executor-only carrier for the opaque physical ULID. It is never parsed as
/// SQL, projected, or persisted; mutation plans need it when a declared `id`
/// shadows the old public physical-id alias.
const PHYSICAL_ROW_ID: &str = "\u{0}elitesql-row-id";

fn current_timestamp_micros() -> Result<i64> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Sql("system clock is before Unix epoch".into()))?
        .as_micros();
    i64::try_from(micros).map_err(|_| Error::Sql("current timestamp is out of range".into()))
}

pub(crate) fn execute(db: &Db, sql: &str) -> Result<QueryOutput> {
    execute_positional(db, sql, &[])
}

pub(crate) fn execute_positional(db: &Db, sql: &str, params: &[Value]) -> Result<QueryOutput> {
    let statement_timestamp = current_timestamp_micros()?;
    let mut statement = parser::parse(sql)?;
    bind_statement(
        &mut statement,
        SuppliedParams::Positional(params),
        statement_timestamp,
    )?;
    execute_statement(db, statement, statement_timestamp)
}

pub(crate) fn execute_named(db: &Db, sql: &str, params: &Record) -> Result<QueryOutput> {
    let statement_timestamp = current_timestamp_micros()?;
    let mut statement = parser::parse(sql)?;
    bind_statement(
        &mut statement,
        SuppliedParams::Named(params),
        statement_timestamp,
    )?;
    execute_statement(db, statement, statement_timestamp)
}

pub(crate) fn execute_txn(txn: &mut Txn, sql: &str) -> Result<QueryOutput> {
    execute_txn_positional(txn, sql, &[])
}

pub(crate) fn execute_txn_positional(
    txn: &mut Txn,
    sql: &str,
    params: &[Value],
) -> Result<QueryOutput> {
    let statement_timestamp = current_timestamp_micros()?;
    let mut statement = parser::parse(sql)?;
    bind_statement(
        &mut statement,
        SuppliedParams::Positional(params),
        statement_timestamp,
    )?;
    execute_txn_statement(txn, statement, statement_timestamp)
}

pub(crate) fn execute_txn_named(txn: &mut Txn, sql: &str, params: &Record) -> Result<QueryOutput> {
    let statement_timestamp = current_timestamp_micros()?;
    let mut statement = parser::parse(sql)?;
    bind_statement(
        &mut statement,
        SuppliedParams::Named(params),
        statement_timestamp,
    )?;
    execute_txn_statement(txn, statement, statement_timestamp)
}

fn execute_statement(
    db: &Db,
    statement: Statement,
    statement_timestamp: i64,
) -> Result<QueryOutput> {
    match statement {
        Statement::CreateTable { name, columns } => {
            let mut cols = Vec::with_capacity(columns.len());
            let mut foreign_keys = Vec::new();
            for c in columns {
                if c.name == ID_COLUMN && c.ty == ColumnType::Text && c.primary_key {
                    // `id text PRIMARY KEY` merely exposes the physical key
                    // that every table already owns; it is not stored twice.
                    continue;
                }
                if let Some(foreign_key) = &c.foreign_key {
                    foreign_keys.push(foreign_key.clone());
                }
                cols.push(column_def_to_column(c)?);
            }
            let mut schema = TableSchema::new(name, cols);
            schema.foreign_keys = foreign_keys;
            db.create_table(schema)?;
            Ok(QueryOutput::None)
        }
        Statement::CreateIndex {
            table,
            column,
            unique,
        } => {
            db.create_index(&table, &column, unique)?;
            Ok(QueryOutput::None)
        }
        Statement::DropTable { name, if_exists } => {
            match db.drop_table(&name) {
                Err(Error::TableNotFound(_)) if if_exists => {}
                other => other?,
            }
            Ok(QueryOutput::None)
        }
        Statement::DropIndex {
            table,
            column,
            if_exists,
        } => {
            match db.drop_index(&table, &column) {
                Err(Error::IndexNotFound { .. }) if if_exists => {}
                other => other?,
            }
            Ok(QueryOutput::None)
        }
        Statement::AddColumn { table, column } => {
            if column.foreign_key.is_some() {
                return Err(Error::Sql(
                    "ALTER TABLE ADD COLUMN REFERENCES is not supported yet; declare the foreign key in CREATE TABLE"
                        .into(),
                ));
            }
            db.add_column(&table, column_def_to_column(column)?)?;
            Ok(QueryOutput::None)
        }
        Statement::DropColumn {
            table,
            column,
            if_exists,
        } => {
            match db.drop_column(&table, &column) {
                Err(Error::ColumnNotFound { .. }) if if_exists => {}
                other => other?,
            }
            Ok(QueryOutput::None)
        }
        Statement::RenameTable { table, to } => {
            db.rename_table(&table, &to)?;
            Ok(QueryOutput::None)
        }
        Statement::RenameColumn { table, column, to } => {
            db.rename_column(&table, &column, &to)?;
            Ok(QueryOutput::None)
        }
        Statement::Insert {
            table,
            columns,
            rows,
            returning,
            ignore_unique,
        } => exec_insert(
            db,
            &table,
            &columns,
            &rows,
            &returning,
            statement_timestamp,
            ignore_unique,
        ),
        Statement::Select(stmt) => exec_select(db, &stmt),
        Statement::Explain(stmt) => explain_select(db, &stmt),
        Statement::Update {
            table,
            sets,
            where_clause,
        } => exec_update(db, &table, &sets, where_clause.as_ref()),
        Statement::Delete {
            table,
            where_clause,
        } => exec_delete(db, &table, where_clause.as_ref()),
    }
}

fn execute_txn_statement(
    txn: &mut Txn,
    statement: Statement,
    statement_timestamp: i64,
) -> Result<QueryOutput> {
    match statement {
        Statement::Insert {
            table,
            columns,
            rows,
            returning,
            ignore_unique,
        } => {
            if ignore_unique {
                return Err(Error::Sql(
                    "ON CONFLICT DO NOTHING is currently autocommit-only".into(),
                ));
            }
            exec_insert_txn(
                txn,
                &table,
                &columns,
                &rows,
                &returning,
                statement_timestamp,
            )
        }
        Statement::Update {
            table,
            sets,
            where_clause,
        } => exec_update_txn(txn, &table, &sets, where_clause.as_ref()),
        Statement::Delete {
            table,
            where_clause,
        } => exec_delete_txn(txn, &table, where_clause.as_ref()),
        Statement::Select(statement) => exec_select_txn(txn, &statement),
        Statement::Explain(_) => Err(Error::Sql(
            "EXPLAIN is not available inside a transaction".into(),
        )),
        _ => Err(Error::Sql(
            "DDL is not allowed inside a transaction; create the schema before BEGIN".into(),
        )),
    }
}

pub(crate) fn execute_cursor<'db>(db: &'db Db, sql: &str) -> Result<QueryCursor<'db>> {
    execute_cursor_positional(db, sql, &[])
}

pub(crate) fn execute_cursor_positional<'db>(
    db: &'db Db,
    sql: &str,
    params: &[Value],
) -> Result<QueryCursor<'db>> {
    execute_cursor_with(db, sql, SuppliedParams::Positional(params))
}

pub(crate) fn execute_cursor_named<'db>(
    db: &'db Db,
    sql: &str,
    params: &Record,
) -> Result<QueryCursor<'db>> {
    execute_cursor_with(db, sql, SuppliedParams::Named(params))
}

fn execute_cursor_with<'db>(
    db: &'db Db,
    sql: &str,
    params: SuppliedParams<'_>,
) -> Result<QueryCursor<'db>> {
    let memory_permit = db.acquire_query_memory();
    let statement_timestamp = current_timestamp_micros()?;
    let mut statement = parser::parse(sql)?;
    bind_statement(&mut statement, params, statement_timestamp)?;
    let Statement::Select(stmt) = statement else {
        return Err(Error::Sql(
            "query_cursor requires a SELECT statement".into(),
        ));
    };
    if !stmt.joins.is_empty() {
        return Err(Error::Sql(
            "streaming JOIN cursors are not implemented yet; use query()".into(),
        ));
    }
    if !stmt.order_by.is_empty() {
        return Err(Error::Sql(
            "streaming ORDER BY cursors are not implemented yet; use query() with spill".into(),
        ));
    }
    if !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt
            .projection
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. }))
    {
        return Err(Error::Sql(
            "streaming aggregate cursors are not implemented yet; use query()".into(),
        ));
    }

    let schema = db
        .table_schema(&stmt.from.name)
        .ok_or_else(|| Error::TableNotFound(stmt.from.name.clone()))?;
    let tables = vec![TableCtx {
        label: stmt
            .from
            .alias
            .clone()
            .unwrap_or_else(|| stmt.from.name.clone()),
        schema,
    }];
    let mut predicates = Vec::new();
    if let Some(expr) = &stmt.where_clause {
        collect_conjuncts(resolve_expr(&tables, expr)?, &mut predicates);
    }
    let (columns, extract) = projection_plan(&tables, &stmt.projection)?;
    let memory = db.memory_options();
    let batch_rows = memory
        .scan_batch_rows
        .min((memory.query_working_bytes / 1024).max(1));
    Ok(QueryCursor {
        _memory: memory_permit,
        db,
        snapshot: db.snapshot(),
        table: stmt.from.name,
        batch_rows,
        after_id: None,
        batch: Vec::new().into_iter(),
        predicates,
        extract,
        columns,
        offset_remaining: limit_to_usize(stmt.offset.as_ref()).unwrap_or(0),
        limit_remaining: limit_to_usize(stmt.limit.as_ref()),
        done: false,
    })
}

#[derive(Clone, Copy)]
enum SuppliedParams<'a> {
    Positional(&'a [Value]),
    Named(&'a Record),
}

struct Binder<'a> {
    supplied: SuppliedParams<'a>,
    positional_index: usize,
    named_used: HashSet<String>,
    statement_timestamp: i64,
}

fn bind_statement(
    statement: &mut Statement,
    supplied: SuppliedParams<'_>,
    statement_timestamp: i64,
) -> Result<()> {
    let mut binder = Binder {
        supplied,
        positional_index: 0,
        named_used: HashSet::new(),
        statement_timestamp,
    };
    match statement {
        Statement::CreateTable { columns, .. } => {
            for column in columns {
                if !matches!(column.default, Some(Literal::CurrentTimestamp)) {
                    binder.bind_optional_literal(&mut column.default)?;
                }
            }
        }
        Statement::Insert { rows, .. } => {
            for row in rows {
                for literal in row {
                    binder.bind_literal(literal)?;
                }
            }
        }
        Statement::Select(select) | Statement::Explain(select) => {
            binder.bind_optional_expr(&mut select.where_clause)?;
            binder.bind_optional_expr(&mut select.having)?;
            binder.bind_optional_limit(&mut select.limit)?;
            binder.bind_optional_limit(&mut select.offset)?;
        }
        Statement::Update {
            sets, where_clause, ..
        } => {
            for (_, value) in sets {
                match value {
                    SetValue::Literal(literal) => binder.bind_literal(literal)?,
                    SetValue::Arithmetic { right, .. } => binder.bind_literal(right)?,
                }
            }
            binder.bind_optional_expr(where_clause)?;
        }
        Statement::Delete { where_clause, .. } => binder.bind_optional_expr(where_clause)?,
        Statement::AddColumn { column, .. } => {
            if !matches!(column.default, Some(Literal::CurrentTimestamp)) {
                binder.bind_optional_literal(&mut column.default)?;
            }
        }
        Statement::CreateIndex { .. }
        | Statement::DropTable { .. }
        | Statement::DropIndex { .. }
        | Statement::DropColumn { .. }
        | Statement::RenameTable { .. }
        | Statement::RenameColumn { .. } => {}
    }
    binder.finish()
}

impl Binder<'_> {
    fn take_positional(&mut self) -> Result<Value> {
        let SuppliedParams::Positional(values) = self.supplied else {
            return Err(Error::InvalidArgument(
                "SQL uses positional placeholders but named parameters were supplied".into(),
            ));
        };
        let Some(value) = values.get(self.positional_index) else {
            return Err(Error::InvalidArgument(format!(
                "missing positional SQL parameter {}",
                self.positional_index + 1
            )));
        };
        self.positional_index += 1;
        Ok(value.clone())
    }

    fn take_named(&mut self, name: &str) -> Result<Value> {
        let SuppliedParams::Named(values) = self.supplied else {
            return Err(Error::InvalidArgument(format!(
                "SQL uses named placeholder %({name})s but positional parameters were supplied"
            )));
        };
        let Some(value) = values.get(name) else {
            return Err(Error::InvalidArgument(format!(
                "missing named SQL parameter '{name}'"
            )));
        };
        self.named_used.insert(name.to_owned());
        Ok(value.clone())
    }

    fn bind_literal(&mut self, literal: &mut Literal) -> Result<()> {
        let value = match literal {
            Literal::PositionalParam => Some(self.take_positional()?),
            Literal::NamedParam(name) => {
                let name = name.clone();
                Some(self.take_named(&name)?)
            }
            Literal::CurrentTimestamp => Some(Value::Timestamp(self.statement_timestamp)),
            _ => None,
        };
        if let Some(value) = value {
            *literal = Literal::Bound(value);
        }
        Ok(())
    }

    fn bind_optional_literal(&mut self, literal: &mut Option<Literal>) -> Result<()> {
        if let Some(literal) = literal {
            self.bind_literal(literal)?;
        }
        Ok(())
    }

    fn bind_optional_limit(&mut self, limit: &mut Option<LimitValue>) -> Result<()> {
        let Some(limit) = limit else { return Ok(()) };
        let value = match limit {
            LimitValue::PositionalParam => Some(self.take_positional()?),
            LimitValue::NamedParam(name) => {
                let name = name.clone();
                Some(self.take_named(&name)?)
            }
            LimitValue::Literal(_) => None,
        };
        if let Some(value) = value {
            let Value::Int64(value) = value else {
                return Err(Error::InvalidArgument(
                    "LIMIT/OFFSET parameter must be a non-negative int64".into(),
                ));
            };
            let value = u64::try_from(value).map_err(|_| {
                Error::InvalidArgument("LIMIT/OFFSET parameter must be non-negative".into())
            })?;
            *limit = LimitValue::Literal(value);
        }
        Ok(())
    }

    fn bind_optional_expr(&mut self, expr: &mut Option<Expr>) -> Result<()> {
        if let Some(expr) = expr {
            self.bind_expr(expr)?;
        }
        Ok(())
    }

    fn bind_expr(&mut self, expr: &mut Expr) -> Result<()> {
        match expr {
            Expr::Cmp { left, right, .. } => {
                self.bind_operand(left)?;
                self.bind_operand(right)?;
            }
            Expr::IsNull { .. } => {}
            Expr::InList { list, .. } => {
                for literal in list {
                    self.bind_literal(literal)?;
                }
            }
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.bind_expr(left)?;
                self.bind_expr(right)?;
            }
            Expr::Not(inner) => self.bind_expr(inner)?,
        }
        Ok(())
    }

    fn bind_operand(&mut self, operand: &mut Operand) -> Result<()> {
        if let Operand::Lit(literal) = operand {
            self.bind_literal(literal)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        match self.supplied {
            SuppliedParams::Positional(values) if self.positional_index != values.len() => {
                Err(Error::InvalidArgument(format!(
                    "{} unused positional SQL parameter(s)",
                    values.len() - self.positional_index
                )))
            }
            SuppliedParams::Named(values) => {
                let unused: Vec<_> = values
                    .keys()
                    .filter(|name| !self.named_used.contains(*name))
                    .cloned()
                    .collect();
                if unused.is_empty() {
                    Ok(())
                } else {
                    Err(Error::InvalidArgument(format!(
                        "unused named SQL parameter(s): {}",
                        unused.join(", ")
                    )))
                }
            }
            _ => Ok(()),
        }
    }
}

fn limit_to_usize(limit: Option<&LimitValue>) -> Option<usize> {
    match limit {
        Some(LimitValue::Literal(value)) => Some(usize::try_from(*value).unwrap_or(usize::MAX)),
        Some(LimitValue::PositionalParam | LimitValue::NamedParam(_)) => {
            unreachable!("parameters are bound before execution")
        }
        None => None,
    }
}

/// Turn a parsed column definition into a schema column, coercing a DEFAULT
/// literal to the column's type so the catalog stores a typed value.
fn column_def_to_column(c: ColumnDef) -> Result<crate::schema::Column> {
    let mut col = match (&c.enum_values, c.max_length, c.dim) {
        (Some(values), _, _) => crate::schema::Column::enumeration(c.name.clone(), values.clone()),
        (None, Some(max_length), _) => crate::schema::Column::varchar(c.name.clone(), max_length),
        (None, None, Some(dim)) => crate::schema::Column::vector(c.name.clone(), dim),
        (None, None, None) => crate::schema::Column::new(c.name.clone(), c.ty),
    };
    if c.not_null {
        col = col.not_null();
    }
    if let Some(lit) = &c.default {
        if matches!(lit, Literal::CurrentTimestamp) {
            if c.ty != ColumnType::Timestamp {
                return Err(Error::Sql(format!(
                    "CURRENT_TIMESTAMP default on '{}' requires a timestamp column",
                    c.name
                )));
            }
            col = col.with_current_timestamp_default();
        } else {
            let value = match lit {
                Literal::Null => Value::Null,
                other => literal_to_value(other, c.ty, &c.name)?,
            };
            col = col.with_default(&value);
        }
    }
    if c.identity {
        col = col.identity();
    }
    Ok(col)
}

// --- literals ------------------------------------------------------------------

/// Schema-aware coercion for INSERT/UPDATE SET values.
fn literal_to_value(lit: &Literal, ty: ColumnType, col: &str) -> Result<Value> {
    if let Literal::Bound(value) = lit {
        return bound_value_for_column(value, ty, col);
    }
    let out = match (lit, ty) {
        (Literal::Null, _) => Value::Null,
        (Literal::Bool(b), ColumnType::Bool) => Value::Bool(*b),
        (Literal::Int(n), ColumnType::Int64) => Value::Int64(*n),
        (Literal::Int(n), ColumnType::Float64) => Value::Float64(*n as f64),
        (Literal::Int(n), ColumnType::Timestamp) => Value::Timestamp(*n),
        (Literal::Float(f), ColumnType::Float64) => Value::Float64(*f),
        (Literal::Str(s), ColumnType::Text) => Value::Text(s.clone()),
        (Literal::Str(s), ColumnType::Json) => {
            let j = serde_json::from_str(s)
                .map_err(|e| Error::Sql(format!("invalid json literal for '{col}': {e}")))?;
            Value::Json(j)
        }
        (Literal::Str(s), ColumnType::Vector) => {
            let v: Vec<f32> = serde_json::from_str(s).map_err(|e| {
                Error::Sql(format!(
                    "invalid vector literal for '{col}' (expected a JSON array of numbers): {e}"
                ))
            })?;
            Value::Vector(v)
        }
        (Literal::Str(s), ColumnType::Timestamp) => Value::parse_timestamp(s).ok_or_else(|| {
            Error::Sql(format!(
                "invalid timestamp literal for '{col}': expected 'YYYY-MM-DD HH:MM:SS[.ffffff]' (UTC)"
            ))
        })?,
        (Literal::Str(s), ColumnType::Date) => Value::parse_date(s).ok_or_else(|| {
            Error::Sql(format!("invalid date literal for '{col}': expected 'YYYY-MM-DD' with a real date"))
        })?,
        (Literal::Str(s), ColumnType::Time) => Value::parse_time(s).ok_or_else(|| {
            Error::Sql(format!("invalid time literal for '{col}': expected 'HH:MM:SS[.ffffff]'"))
        })?,
        (Literal::Int(n), ColumnType::Date) => {
            let d = i32::try_from(*n).map_err(|_| {
                Error::Sql(format!("date literal out of range for '{col}'"))
            })?;
            Value::Date(d)
        }
        (Literal::Int(n), ColumnType::Time) => {
            if !(0..86_400_000_000).contains(n) {
                return Err(Error::Sql(format!(
                    "time literal out of range for '{col}': 0..86400000000 microseconds"
                )));
            }
            Value::Time(*n)
        }
        (Literal::Blob(b), ColumnType::Blob) => Value::Blob(b.clone()),
        (
            Literal::PositionalParam
            | Literal::NamedParam(_)
            | Literal::Bound(_)
            | Literal::CurrentTimestamp,
            _,
        ) => {
            unreachable!("parameters are bound before execution")
        }
        _ => {
            return Err(Error::Sql(format!(
                "literal {lit:?} is not valid for column '{col}' of type {ty}"
            )))
        }
    };
    Ok(out)
}

fn bound_value_for_column(value: &Value, ty: ColumnType, col: &str) -> Result<Value> {
    if value.is_null() || value.matches(ty) {
        return Ok(value.clone());
    }
    let converted = match (value, ty) {
        (Value::Int64(value), ColumnType::Float64) => Some(Value::Float64(*value as f64)),
        (Value::Int64(value), ColumnType::Timestamp) => Some(Value::Timestamp(*value)),
        (Value::Int64(value), ColumnType::Date) => {
            Some(Value::Date(i32::try_from(*value).map_err(|_| {
                Error::Sql(format!("date parameter out of range for '{col}'"))
            })?))
        }
        (Value::Int64(value), ColumnType::Time) if (0..86_400_000_000).contains(value) => {
            Some(Value::Time(*value))
        }
        (Value::Text(value), ColumnType::Timestamp) => Value::parse_timestamp(value),
        (Value::Text(value), ColumnType::Date) => Value::parse_date(value),
        (Value::Text(value), ColumnType::Time) => Value::parse_time(value),
        (Value::Text(value), ColumnType::Json) => {
            Some(Value::Json(serde_json::Value::String(value.clone())))
        }
        (Value::Bool(value), ColumnType::Json) => Some(Value::Json((*value).into())),
        (Value::Int64(value), ColumnType::Json) => Some(Value::Json((*value).into())),
        (Value::Float64(value), ColumnType::Json) => {
            serde_json::Number::from_f64(*value).map(|number| Value::Json(number.into()))
        }
        (Value::Json(serde_json::Value::Array(values)), ColumnType::Vector) => {
            let mut vector = Vec::with_capacity(values.len());
            for component in values {
                let Some(component) = component.as_f64() else {
                    return Err(Error::Sql(format!(
                        "vector parameter for '{col}' contains a non-numeric component"
                    )));
                };
                vector.push(component as f32);
            }
            Some(Value::Vector(vector))
        }
        _ => None,
    };
    converted.ok_or_else(|| {
        Error::Sql(format!(
            "parameter value {value:?} is not valid for column '{col}' of type {ty}"
        ))
    })
}

/// Context-free conversion for WHERE comparisons.
fn literal_to_plain_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Int(n) => Value::Int64(*n),
        Literal::Float(f) => Value::Float64(*f),
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Bound(value) => value.clone(),
        Literal::PositionalParam | Literal::NamedParam(_) | Literal::CurrentTimestamp => {
            unreachable!("parameters are bound before expression resolution")
        }
    }
}

// --- value comparison ------------------------------------------------------------

fn numeric(v: &Value) -> Option<f64> {
    match v {
        Value::Int64(n) => Some(*n as f64),
        Value::Float64(f) => Some(*f),
        Value::Timestamp(t) => Some(*t as f64),
        _ => None,
    }
}

/// Strict ordering for WHERE comparisons; None = not comparable.
/// Text literals coerce against date/time columns ('2026-08-07', '09:30:00')
/// so natural predicates work without a cast syntax.
fn cmp_vals(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::Time(x), Value::Time(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Text(s)) => match Value::parse_date(s) {
            Some(Value::Date(y)) => Some(x.cmp(&y)),
            _ => None,
        },
        (Value::Text(s), Value::Date(y)) => match Value::parse_date(s) {
            Some(Value::Date(x)) => Some(x.cmp(y)),
            _ => None,
        },
        (Value::Time(x), Value::Text(s)) => match Value::parse_time(s) {
            Some(Value::Time(y)) => Some(x.cmp(&y)),
            _ => None,
        },
        (Value::Text(s), Value::Time(y)) => match Value::parse_time(s) {
            Some(Value::Time(x)) => Some(x.cmp(y)),
            _ => None,
        },
        (Value::Timestamp(x), Value::Text(s)) => match Value::parse_timestamp(s) {
            Some(Value::Timestamp(y)) => Some(x.cmp(&y)),
            _ => None,
        },
        (Value::Text(s), Value::Timestamp(y)) => match Value::parse_timestamp(s) {
            Some(Value::Timestamp(x)) => Some(x.cmp(y)),
            _ => None,
        },
        _ => match (numeric(a), numeric(b)) {
            (Some(x), Some(y)) => Some(x.total_cmp(&y)),
            _ => None,
        },
    }
}

fn eq_vals(a: &Value, b: &Value) -> Option<bool> {
    if let (Value::Json(x), Value::Json(y)) = (a, b) {
        return Some(x == y);
    }
    cmp_vals(a, b).map(|o| o == Ordering::Equal)
}

/// Total order for ORDER BY: never fails, NULLs first, then by type family.
fn sort_cmp(a: &Value, b: &Value, collation: Collation) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int64(_) | Value::Float64(_) | Value::Timestamp(_) => 2,
            Value::Text(_) => 3,
            Value::Blob(_) => 4,
            Value::Json(_) => 5,
            Value::Vector(_) => 6,
            Value::Date(_) => 7,
            Value::Time(_) => 8,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => collation.compare(x, y),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        (Value::Json(x), Value::Json(y)) => x.to_string().cmp(&y.to_string()),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        _ => numeric(a)
            .zip(numeric(b))
            .map(|(x, y)| x.total_cmp(&y))
            .unwrap_or(Ordering::Equal),
    }
}

// --- resolved expressions -----------------------------------------------------

#[derive(Debug, Clone)]
enum RVal {
    Col(usize, String),
    Val(Value),
    /// Index into the query's aggregate list; only produced for HAVING.
    Agg(usize),
}

#[derive(Debug, Clone)]
enum RExpr {
    Cmp {
        left: RVal,
        op: CmpOp,
        right: RVal,
    },
    IsNull {
        col: (usize, String),
        negated: bool,
    },
    InList {
        col: (usize, String),
        list: Vec<Value>,
        negated: bool,
    },
    And(Box<RExpr>, Box<RExpr>),
    Or(Box<RExpr>, Box<RExpr>),
    Not(Box<RExpr>),
}

struct TableCtx {
    schema: TableSchema,
    label: String,
}

fn resolve_col(tables: &[TableCtx], cref: &ColumnRef) -> Result<(usize, String)> {
    if let Some(q) = &cref.table {
        for (i, t) in tables.iter().enumerate() {
            if t.label.eq_ignore_ascii_case(q) || t.schema.name.eq_ignore_ascii_case(q) {
                check_col(&t.schema, &cref.column)?;
                return Ok((i, cref.column.clone()));
            }
        }
        return Err(Error::Sql(format!("unknown table or alias '{q}'")));
    }
    let mut found: Option<usize> = None;
    for (i, t) in tables.iter().enumerate() {
        if has_col(&t.schema, &cref.column) {
            if found.is_some() {
                return Err(Error::Sql(format!(
                    "column '{}' is ambiguous; qualify it (t.{})",
                    cref.column, cref.column
                )));
            }
            found = Some(i);
        }
    }
    match found {
        Some(i) => Ok((i, cref.column.clone())),
        None => Err(Error::Sql(format!("unknown column '{}'", cref.column))),
    }
}

fn has_col(schema: &TableSchema, name: &str) -> bool {
    schema.column(name).is_some() || (name == ID_COLUMN && schema.has_implicit_id())
}

fn check_col(schema: &TableSchema, name: &str) -> Result<()> {
    if has_col(schema, name) {
        Ok(())
    } else {
        Err(Error::Sql(format!(
            "unknown column '{}' in table '{}'",
            name, schema.name
        )))
    }
}

fn resolve_expr(tables: &[TableCtx], expr: &Expr) -> Result<RExpr> {
    Ok(match expr {
        Expr::Cmp { left, op, right } => RExpr::Cmp {
            left: resolve_operand(tables, left)?,
            op: *op,
            right: resolve_operand(tables, right)?,
        },
        Expr::IsNull { col, negated } => RExpr::IsNull {
            col: resolve_col(tables, col)?,
            negated: *negated,
        },
        Expr::InList { col, list, negated } => RExpr::InList {
            col: resolve_col(tables, col)?,
            list: list.iter().map(literal_to_plain_value).collect(),
            negated: *negated,
        },
        Expr::And(a, b) => RExpr::And(
            Box::new(resolve_expr(tables, a)?),
            Box::new(resolve_expr(tables, b)?),
        ),
        Expr::Or(a, b) => RExpr::Or(
            Box::new(resolve_expr(tables, a)?),
            Box::new(resolve_expr(tables, b)?),
        ),
        Expr::Not(e) => RExpr::Not(Box::new(resolve_expr(tables, e)?)),
    })
}

fn resolve_operand(tables: &[TableCtx], op: &Operand) -> Result<RVal> {
    Ok(match op {
        Operand::Col(c) => {
            let (i, name) = resolve_col(tables, c)?;
            RVal::Col(i, name)
        }
        Operand::Lit(l) => RVal::Val(literal_to_plain_value(l)),
        Operand::Agg { .. } => {
            return Err(Error::Sql(
                "aggregates are only allowed in the SELECT list and HAVING".into(),
            ))
        }
    })
}

type ExecRow = Vec<Option<Record>>;

fn col_value(row: &ExecRow, ti: usize, col: &str) -> Value {
    row.get(ti)
        .and_then(|r| r.as_ref())
        .and_then(|r| r.get(col).cloned())
        .unwrap_or(Value::Null)
}

/// Evaluation context for HAVING: grouped column values + aggregate results.
type HavingCtx<'a> = (&'a HashMap<(usize, String), Value>, &'a [Value]);

fn rval_value(row: &ExecRow, rv: &RVal, having: Option<HavingCtx>) -> Value {
    match rv {
        RVal::Col(ti, col) => match having {
            Some((groups, _)) => groups
                .get(&(*ti, col.clone()))
                .cloned()
                .unwrap_or(Value::Null),
            None => col_value(row, *ti, col),
        },
        RVal::Val(v) => v.clone(),
        RVal::Agg(i) => having
            .and_then(|(_, aggs)| aggs.get(*i).cloned())
            .unwrap_or(Value::Null),
    }
}

fn ctx_col_value(row: &ExecRow, col: &(usize, String), having: Option<HavingCtx>) -> Value {
    match having {
        Some((groups, _)) => groups.get(col).cloned().unwrap_or(Value::Null),
        None => col_value(row, col.0, &col.1),
    }
}

/// SQL three-valued logic. A comparison against NULL is `Unknown`, not false,
/// and only `True` keeps a row: `WHERE NOT col = 'x'` therefore drops NULL rows
/// the way standard SQL requires, instead of admitting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn of(value: bool) -> Truth {
        if value {
            Truth::True
        } else {
            Truth::False
        }
    }

    fn is_true(self) -> bool {
        self == Truth::True
    }

    fn not(self) -> Truth {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }

    fn and(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::True,
        }
    }

    fn or(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::False,
        }
    }
}

fn eval(row: &ExecRow, e: &RExpr) -> Result<Truth> {
    eval_ctx(row, e, None)
}

fn eval_ctx(row: &ExecRow, e: &RExpr, having: Option<HavingCtx>) -> Result<Truth> {
    Ok(match e {
        RExpr::Cmp { left, op, right } => {
            let a = rval_value(row, left, having);
            let b = rval_value(row, right, having);
            if a.is_null() || b.is_null() {
                return Ok(Truth::Unknown);
            }
            Truth::of(match op {
                CmpOp::Eq | CmpOp::Neq => {
                    let eq = eq_vals(&a, &b).ok_or_else(|| not_comparable(&a, &b))?;
                    if *op == CmpOp::Eq {
                        eq
                    } else {
                        !eq
                    }
                }
                _ => {
                    let ord = cmp_vals(&a, &b).ok_or_else(|| not_comparable(&a, &b))?;
                    match op {
                        CmpOp::Lt => ord == Ordering::Less,
                        CmpOp::Le => ord != Ordering::Greater,
                        CmpOp::Gt => ord == Ordering::Greater,
                        CmpOp::Ge => ord != Ordering::Less,
                        _ => unreachable!(),
                    }
                }
            })
        }
        // IS NULL is the one test that always knows its answer.
        RExpr::IsNull { col, negated } => {
            let v = ctx_col_value(row, col, having);
            Truth::of(v.is_null() != *negated)
        }
        RExpr::InList { col, list, negated } => {
            let v = ctx_col_value(row, col, having);
            if v.is_null() {
                return Ok(Truth::Unknown);
            }
            let mut hit = false;
            let mut saw_null = false;
            for item in list {
                if item.is_null() {
                    saw_null = true;
                    continue;
                }
                if eq_vals(&v, item).ok_or_else(|| not_comparable(&v, item))? {
                    hit = true;
                    break;
                }
            }
            // No match plus a NULL in the list is Unknown, not false: the NULL
            // might have been the match. This is what makes `NOT IN (1, NULL)`
            // return nothing, as standard SQL requires.
            let found = if hit {
                Truth::True
            } else if saw_null {
                Truth::Unknown
            } else {
                Truth::False
            };
            if *negated {
                found.not()
            } else {
                found
            }
        }
        // Short-circuit on the decisive value, so a NULL-comparison branch
        // never has to be evaluated (and cannot raise a comparison error) once
        // the result is already settled.
        RExpr::And(a, b) => {
            let left = eval_ctx(row, a, having)?;
            if left == Truth::False {
                return Ok(Truth::False);
            }
            left.and(eval_ctx(row, b, having)?)
        }
        RExpr::Or(a, b) => {
            let left = eval_ctx(row, a, having)?;
            if left == Truth::True {
                return Ok(Truth::True);
            }
            left.or(eval_ctx(row, b, having)?)
        }
        RExpr::Not(inner) => eval_ctx(row, inner, having)?.not(),
    })
}

fn not_comparable(a: &Value, b: &Value) -> Error {
    Error::Sql(format!("cannot compare {a:?} with {b:?}"))
}

// --- conjuncts & table access -----------------------------------------------------

fn collect_conjuncts(e: RExpr, out: &mut Vec<RExpr>) {
    match e {
        RExpr::And(a, b) => {
            collect_conjuncts(*a, out);
            collect_conjuncts(*b, out);
        }
        other => out.push(other),
    }
}

fn tables_referenced(e: &RExpr, out: &mut Vec<usize>) {
    match e {
        RExpr::Cmp { left, right, .. } => {
            for rv in [left, right] {
                if let RVal::Col(i, _) = rv {
                    out.push(*i);
                }
            }
        }
        RExpr::IsNull { col, .. } | RExpr::InList { col, .. } => out.push(col.0),
        RExpr::And(a, b) | RExpr::Or(a, b) => {
            tables_referenced(a, out);
            tables_referenced(b, out);
        }
        RExpr::Not(inner) => tables_referenced(inner, out),
    }
}

fn single_table_of(e: &RExpr) -> Option<usize> {
    let mut refs = Vec::new();
    tables_referenced(e, &mut refs);
    refs.sort_unstable();
    refs.dedup();
    match refs.as_slice() {
        [one] => Some(*one),
        [] => None, // constant-ish predicate: keep as residual
        _ => None,
    }
}

/// Coerce a plain value to a column's type for equality lookups.
/// Returns None when the coercion is lossy or impossible (fall back to scan).
fn coerce_for_lookup(v: &Value, ty: ColumnType) -> Option<Value> {
    match (v, ty) {
        (Value::Int64(n), ColumnType::Timestamp) => Some(Value::Timestamp(*n)),
        (Value::Int64(n), ColumnType::Float64) => Some(Value::Float64(*n as f64)),
        (Value::Text(s), ColumnType::Date) => Value::parse_date(s),
        (Value::Text(s), ColumnType::Time) => Value::parse_time(s),
        (Value::Text(s), ColumnType::Timestamp) => Value::parse_timestamp(s),
        _ if v.matches(ty) => Some(v.clone()),
        _ => None,
    }
}

/// Fetch a table's rows applying its pushed-down conjuncts. The access path is
/// `table_driver`'s decision, the same one the batched paths and `EXPLAIN` use;
/// this variant materializes the whole result for the callers that need it.
fn fetch_table(db: &Db, ctx: &TableCtx, ti: usize, conjuncts: &[RExpr]) -> Result<Vec<Record>> {
    let rows = match table_driver(ctx, ti, conjuncts) {
        TableDriver::Empty => Vec::new(),
        TableDriver::Id(id) => db
            .get_unbudgeted(&ctx.schema.name, &id)?
            .into_iter()
            .map(|record| (id.clone(), record))
            .collect(),
        // find_eq selects a secondary index when one exists and otherwise uses
        // the segment-streaming equality scan. Either path narrows the rows
        // before the remaining conjuncts are evaluated.
        TableDriver::Equality(column, key) => db
            .find_eq_unbudgeted(&ctx.schema.name, &column, &key)?
            .into_iter()
            .collect(),
        TableDriver::Scan => db.scan_unbudgeted(&ctx.schema.name)?.into_iter().collect(),
    };
    // Apply every conjunct (re-checking the driving one is harmless).
    let mut out = Vec::with_capacity(rows.len());
    for (id, mut r) in rows {
        r.insert(PHYSICAL_ROW_ID.into(), Value::Text(id));
        let row: ExecRow = single_row(ti, r);
        if eval_all(&row, conjuncts)? {
            out.push(take_single(row, ti));
        }
    }
    Ok(out)
}

fn physical_row_id(record: &Record) -> Result<&str> {
    match record.get(PHYSICAL_ROW_ID) {
        Some(Value::Text(id)) => Ok(id),
        _ => Err(Error::Corrupt(
            "executor row is missing its physical id".into(),
        )),
    }
}

fn single_row(ti: usize, rec: Record) -> ExecRow {
    let mut row: ExecRow = vec![None; ti + 1];
    row[ti] = Some(rec);
    row
}

fn take_single(mut row: ExecRow, ti: usize) -> Record {
    row[ti].take().expect("present")
}

/// A row survives a filter only when every conjunct is `True`; `Unknown` keeps
/// nothing, which is what makes NULLs fall out of a WHERE clause.
fn eval_all(row: &ExecRow, conjuncts: &[RExpr]) -> Result<bool> {
    for c in conjuncts {
        if !eval(row, c)?.is_true() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn join_key(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    // Normalize numeric families so int64/timestamp/float compare consistently
    // as join keys only when exactly equal in value.
    let norm = match v {
        Value::Timestamp(t) => Value::Int64(*t),
        other => other.clone(),
    };
    let mut buf = Vec::new();
    encode_value(&mut buf, &norm);
    Some(buf)
}

#[derive(Debug)]
struct SortedOutputRow {
    keys: Vec<Value>,
    values: Vec<Value>,
    sequence: u64,
}

/// How one ORDER BY key is compared: direction plus, for text, which
/// collation decides the order.
#[derive(Debug, Clone, Copy)]
struct SortSpec {
    desc: bool,
    collation: Collation,
}

impl SortSpec {
    fn ascending() -> SortSpec {
        SortSpec {
            desc: false,
            collation: Collation::default(),
        }
    }
}

fn compare_sorted_rows(a: &SortedOutputRow, b: &SortedOutputRow, specs: &[SortSpec]) -> Ordering {
    for ((av, bv), spec) in a.keys.iter().zip(&b.keys).zip(specs) {
        let ord = sort_cmp(av, bv, spec.collation);
        if ord != Ordering::Equal {
            return if spec.desc { ord.reverse() } else { ord };
        }
    }
    a.sequence.cmp(&b.sequence)
}

fn value_heap_bytes(value: &Value) -> usize {
    size_of::<Value>()
        + match value {
            Value::Text(s) => s.capacity(),
            Value::Blob(b) => b.capacity(),
            Value::Vector(v) => v.capacity() * size_of::<f32>(),
            Value::Json(v) => v.to_string().len(),
            _ => 0,
        }
}

fn sorted_row_bytes(row: &SortedOutputRow) -> usize {
    size_of::<SortedOutputRow>()
        + row.keys.capacity() * size_of::<Value>()
        + row.values.capacity() * size_of::<Value>()
        + row.keys.iter().map(value_heap_bytes).sum::<usize>()
        + row.values.iter().map(value_heap_bytes).sum::<usize>()
}

/// Owns spill paths so every early-return/error path removes its temporary
/// files. Files are intentionally outside the database format and never need
/// recovery.
struct SpillFiles(Vec<PathBuf>);

impl Drop for SpillFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct SpillSorter<'a> {
    db: &'a Db,
    budget: usize,
    keep: Option<usize>,
    specs: Vec<SortSpec>,
    buffer: Vec<SortedOutputRow>,
    buffer_bytes: usize,
    spill_dir: PathBuf,
    runs: SpillFiles,
}

impl<'a> SpillSorter<'a> {
    fn new(db: &'a Db, specs: Vec<SortSpec>, keep: Option<usize>) -> Result<Self> {
        let memory = db.memory_options();
        let spill_dir = memory
            .spill_directory
            .unwrap_or_else(|| std::env::temp_dir().join("elitesql-query-spill"));
        Ok(Self {
            db,
            budget: memory.query_working_bytes,
            keep,
            specs,
            buffer: Vec::new(),
            buffer_bytes: 0,
            spill_dir,
            runs: SpillFiles(Vec::new()),
        })
    }

    fn push(&mut self, row: SortedOutputRow) -> Result<()> {
        let bytes = sorted_row_bytes(&row);
        // One oversized row is allowed through by itself: the operator never
        // creates a second full-size copy before flushing it.
        if !self.buffer.is_empty() && self.buffer_bytes.saturating_add(bytes) > self.budget {
            self.flush_run()?;
        }
        self.buffer_bytes = self.buffer_bytes.saturating_add(bytes);
        self.buffer.push(row);
        self.db.record_query_buffer(self.buffer_bytes);
        if self.buffer_bytes >= self.budget {
            self.flush_run()?;
        }
        Ok(())
    }

    fn sort_and_prune(&mut self) {
        self.buffer
            .sort_by(|a, b| compare_sorted_rows(a, b, &self.specs));
        if let Some(keep) = self.keep {
            self.buffer.truncate(keep);
        }
    }

    fn flush_run(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.sort_and_prune();
        fs::create_dir_all(&self.spill_dir)?;
        let path = self.spill_dir.join(format!("query-{}.run", Ulid::new()));
        let file = File::create(&path)?;
        let mut writer = BufWriter::new(file);
        for row in &self.buffer {
            write_sorted_row(&mut writer, row)?;
        }
        writer.flush()?;
        let bytes = writer.get_ref().metadata()?.len();
        self.db.record_query_spill(bytes);
        self.runs.0.push(path);
        self.buffer.clear();
        self.buffer_bytes = 0;
        Ok(())
    }

    fn finish(mut self, offset: usize, limit: Option<usize>) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::with_capacity(limit.unwrap_or(0).min(4096));
        self.for_each_sorted(offset, limit, |row| {
            out.push(row.values);
            Ok(())
        })?;
        Ok(out)
    }

    fn for_each_sorted(
        &mut self,
        offset: usize,
        limit: Option<usize>,
        mut visit: impl FnMut(SortedOutputRow) -> Result<()>,
    ) -> Result<()> {
        if self.runs.0.is_empty() {
            self.sort_and_prune();
            let take = limit.unwrap_or(usize::MAX);
            for row in self.buffer.drain(..).skip(offset).take(take) {
                visit(row)?;
            }
            return Ok(());
        }
        self.flush_run()?;

        let mut readers: Vec<SpillRunReader> = self
            .runs
            .0
            .iter()
            .map(SpillRunReader::open)
            .collect::<Result<_>>()?;
        let mut heads: Vec<Option<SortedOutputRow>> = readers
            .iter_mut()
            .map(SpillRunReader::next_row)
            .collect::<Result<_>>()?;
        let take = limit.unwrap_or(usize::MAX);
        let stop = offset.saturating_add(take);
        let mut seen = 0usize;
        while seen < stop {
            let next = heads
                .iter()
                .enumerate()
                .filter_map(|(i, row)| row.as_ref().map(|r| (i, r)))
                .min_by(|(_, a), (_, b)| compare_sorted_rows(a, b, &self.specs))
                .map(|(i, _)| i);
            let Some(run) = next else { break };
            let row = heads[run].take().expect("selected run has a row");
            if seen >= offset {
                visit(row)?;
            }
            seen += 1;
            heads[run] = readers[run].next_row()?;
        }
        Ok(())
    }
}

struct SpillRunReader {
    reader: BufReader<File>,
}

impl SpillRunReader {
    fn open(path: &PathBuf) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
        })
    }

    fn next_row(&mut self) -> Result<Option<SortedOutputRow>> {
        let mut len_bytes = [0u8; 4];
        let mut read = 0usize;
        while read < len_bytes.len() {
            let n = self.reader.read(&mut len_bytes[read..])?;
            if n == 0 {
                if read == 0 {
                    return Ok(None);
                }
                return Err(Error::Corrupt("truncated query spill frame".into()));
            }
            read += n;
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body)?;
        decode_sorted_row(&body).map(Some)
    }
}

fn write_sorted_row(writer: &mut impl Write, row: &SortedOutputRow) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&row.sequence.to_le_bytes());
    body.extend_from_slice(&(row.keys.len() as u32).to_le_bytes());
    for value in &row.keys {
        encode_value(&mut body, value);
    }
    body.extend_from_slice(&(row.values.len() as u32).to_le_bytes());
    for value in &row.values {
        encode_value(&mut body, value);
    }
    let len = u32::try_from(body.len())
        .map_err(|_| Error::Sql("one query row is too large to spill".into()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    Ok(())
}

fn decode_sorted_row(body: &[u8]) -> Result<SortedOutputRow> {
    let mut pos = 0usize;
    let sequence = read_spill_u64(body, &mut pos)?;
    let key_count = read_spill_u32(body, &mut pos)? as usize;
    let mut keys = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        keys.push(decode_value(body, &mut pos, None)?);
    }
    let value_count = read_spill_u32(body, &mut pos)? as usize;
    let mut values = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        values.push(decode_value(body, &mut pos, None)?);
    }
    if pos != body.len() {
        return Err(Error::Corrupt(
            "query spill frame has trailing bytes".into(),
        ));
    }
    Ok(SortedOutputRow {
        keys,
        values,
        sequence,
    })
}

fn read_spill_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let bytes: [u8; 4] = buf
        .get(*pos..pos.saturating_add(4))
        .ok_or_else(|| Error::Corrupt("truncated query spill frame".into()))?
        .try_into()
        .expect("four bytes");
    *pos += 4;
    Ok(u32::from_le_bytes(bytes))
}

fn read_spill_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let bytes: [u8; 8] = buf
        .get(*pos..pos.saturating_add(8))
        .ok_or_else(|| Error::Corrupt("truncated query spill frame".into()))?
        .try_into()
        .expect("eight bytes");
    *pos += 8;
    Ok(u64::from_le_bytes(bytes))
}

fn record_heap_bytes(record: &Record) -> usize {
    size_of::<Record>()
        + record
            .iter()
            .map(|(name, value)| name.capacity() + value_heap_bytes(value))
            .sum::<usize>()
}

fn exec_row_heap_bytes(row: &ExecRow) -> usize {
    size_of::<ExecRow>()
        + row.capacity() * size_of::<Option<Record>>()
        + row
            .iter()
            .filter_map(Option::as_ref)
            .map(record_heap_bytes)
            .sum::<usize>()
}

fn encode_spill_record(record: &Record, out: &mut Vec<u8>) -> Result<()> {
    let count = u32::try_from(record.len())
        .map_err(|_| Error::Sql("too many columns in temporary query row".into()))?;
    out.extend_from_slice(&count.to_le_bytes());
    for (name, value) in record {
        let len = u32::try_from(name.len())
            .map_err(|_| Error::Sql("column name too large for temporary query row".into()))?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        encode_value(out, value);
    }
    Ok(())
}

fn decode_spill_record(buf: &[u8], pos: &mut usize) -> Result<Record> {
    let count = read_spill_u32(buf, pos)? as usize;
    let mut record = Record::new();
    for _ in 0..count {
        let len = read_spill_u32(buf, pos)? as usize;
        let end = pos
            .checked_add(len)
            .filter(|end| *end <= buf.len())
            .ok_or_else(|| Error::Corrupt("truncated query spill column".into()))?;
        let name = std::str::from_utf8(&buf[*pos..end])
            .map_err(|_| Error::Corrupt("invalid utf8 in query spill column".into()))?
            .to_owned();
        *pos = end;
        record.insert(name, decode_value(buf, pos, None)?);
    }
    Ok(record)
}

fn write_spill_exec_row(writer: &mut File, row: &ExecRow) -> Result<u64> {
    let mut body = Vec::new();
    body.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for record in row {
        match record {
            Some(record) => {
                body.push(1);
                encode_spill_record(record, &mut body)?;
            }
            None => body.push(0),
        }
    }
    write_spill_frame(writer, &body)
}

fn write_spill_record(writer: &mut File, record: &Record) -> Result<u64> {
    let mut body = Vec::new();
    encode_spill_record(record, &mut body)?;
    write_spill_frame(writer, &body)
}

fn write_spill_frame(writer: &mut File, body: &[u8]) -> Result<u64> {
    let len = u32::try_from(body.len())
        .map_err(|_| Error::Sql("one query row is too large to spill".into()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(body)?;
    Ok(body.len() as u64 + 4)
}

struct SpillFrameReader {
    reader: BufReader<File>,
}

impl SpillFrameReader {
    fn open(path: &PathBuf) -> Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(4096, File::open(path)?),
        })
    }

    fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len_bytes = [0u8; 4];
        let mut read = 0usize;
        while read < len_bytes.len() {
            let n = self.reader.read(&mut len_bytes[read..])?;
            if n == 0 {
                if read == 0 {
                    return Ok(None);
                }
                return Err(Error::Corrupt("truncated query spill frame".into()));
            }
            read += n;
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut body = vec![0; len];
        self.reader.read_exact(&mut body)?;
        Ok(Some(body))
    }

    fn next_exec_row(&mut self) -> Result<Option<ExecRow>> {
        let Some(body) = self.next_frame()? else {
            return Ok(None);
        };
        let mut pos = 0usize;
        let count = read_spill_u32(&body, &mut pos)? as usize;
        let mut row = Vec::with_capacity(count);
        for _ in 0..count {
            let present = *body
                .get(pos)
                .ok_or_else(|| Error::Corrupt("truncated query spill row".into()))?;
            pos += 1;
            row.push(if present == 0 {
                None
            } else {
                Some(decode_spill_record(&body, &mut pos)?)
            });
        }
        if pos != body.len() {
            return Err(Error::Corrupt("query spill row has trailing bytes".into()));
        }
        Ok(Some(row))
    }

    fn next_record(&mut self) -> Result<Option<Record>> {
        let Some(body) = self.next_frame()? else {
            return Ok(None);
        };
        let mut pos = 0usize;
        let record = decode_spill_record(&body, &mut pos)?;
        if pos != body.len() {
            return Err(Error::Corrupt(
                "query spill record has trailing bytes".into(),
            ));
        }
        Ok(Some(record))
    }
}

type ProjectionPlan = (Vec<String>, Vec<(usize, String)>);

fn implicit_id_and_declared_columns(schema: &TableSchema) -> Vec<String> {
    let mut columns =
        Vec::with_capacity(schema.columns.len() + usize::from(schema.has_implicit_id()));
    if schema.has_implicit_id() {
        columns.push(ID_COLUMN.to_owned());
    }
    columns.extend(schema.columns.iter().map(|column| column.name.clone()));
    columns
}

fn projection_plan(tables: &[TableCtx], projection: &[SelectItem]) -> Result<ProjectionPlan> {
    let single = tables.len() == 1;
    let mut columns = Vec::new();
    let mut extract = Vec::new();
    for item in projection {
        match item {
            SelectItem::Star => {
                for (ti, table) in tables.iter().enumerate() {
                    let prefix = if single {
                        String::new()
                    } else {
                        format!("{}.", table.label)
                    };
                    if table.schema.has_implicit_id() {
                        columns.push(format!("{prefix}id"));
                        extract.push((ti, "id".into()));
                    }
                    for column in &table.schema.columns {
                        columns.push(format!("{prefix}{}", column.name));
                        extract.push((ti, column.name.clone()));
                    }
                }
            }
            SelectItem::Column { col, alias } => {
                let (ti, name) = resolve_col(tables, col)?;
                columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                extract.push((ti, name));
            }
            SelectItem::Aggregate { .. } => {
                return Err(Error::Sql(
                    "aggregate projection reached non-aggregate executor".into(),
                ));
            }
        }
    }
    Ok((columns, extract))
}

fn project_row(row: &ExecRow, extract: &[(usize, String)]) -> Vec<Value> {
    extract
        .iter()
        .map(|(ti, col)| col_value(row, *ti, col))
        .collect()
}

/// How a table's rows are produced. This is the whole access-path decision:
/// every read path (batched SELECT, joins, UPDATE/DELETE) and `EXPLAIN` derive
/// it from `table_driver`, so what EXPLAIN prints is what the executor runs.
enum TableDriver {
    /// The predicate cannot match any row, so nothing is read at all.
    Empty,
    Id(String),
    Equality(String, Value),
    Scan,
}

fn table_driver(table: &TableCtx, ti: usize, predicates: &[RExpr]) -> TableDriver {
    table_driver_at(table, ti, predicates).0
}

/// `table_driver` plus the index of the predicate that drove the choice.
/// Execution re-checks every predicate anyway, so only `EXPLAIN` needs it — to
/// avoid echoing the driving predicate as a redundant filter line.
fn table_driver_at(
    table: &TableCtx,
    ti: usize,
    predicates: &[RExpr],
) -> (TableDriver, Option<usize>) {
    for (position, predicate) in predicates.iter().enumerate() {
        let RExpr::Cmp {
            left,
            op: CmpOp::Eq,
            right,
        } = predicate
        else {
            continue;
        };
        let (column, value) = match (left, right) {
            (RVal::Col(index, column), RVal::Val(value)) if *index == ti => (column, value),
            (RVal::Val(value), RVal::Col(index, column)) if *index == ti => (column, value),
            _ => continue,
        };
        // `col = NULL` is never true, so no access path can produce a row.
        // Answering without touching the table is both faster and what makes
        // the plan honest about it.
        if value.is_null() {
            return (TableDriver::Empty, Some(position));
        }
        if column == ID_COLUMN && table.schema.has_implicit_id() {
            return match value {
                Value::Text(id) => (TableDriver::Id(id.clone()), Some(position)),
                _ => (TableDriver::Empty, Some(position)),
            };
        }
        let ty = table.schema.column(column).expect("resolved column").ty;
        if let Some(value) = coerce_for_lookup(value, ty) {
            return (TableDriver::Equality(column.clone(), value), Some(position));
        }
    }
    (TableDriver::Scan, None)
}

fn has_secondary_index(table: &TableCtx, column: &str) -> bool {
    table.schema.indexes.iter().any(|d| d.column == column)
}

/// True when `column` on `table` can be probed directly instead of scanned.
fn column_is_probeable(table: &TableCtx, column: &str) -> bool {
    (column == ID_COLUMN && table.schema.has_implicit_id()) || has_secondary_index(table, column)
}

/// The join-strategy decision, shared by the executor and `EXPLAIN`.
///
/// Index nested-loop is preferred at every cardinality: unlike the hash path
/// it does not materialize the complete right table and its hash map. The
/// optimizer may later add a cost-based crossover constrained by memory.
/// RIGHT JOIN always takes the hash path, which is what preserves its side.
fn join_uses_index_loop(kind: JoinKind, new_table: &TableCtx, new_col: &str) -> bool {
    kind != JoinKind::Right && column_is_probeable(new_table, new_col)
}

fn driven_batch(
    db: &Db,
    snapshot: &Snapshot,
    table: &TableCtx,
    driver: &TableDriver,
    after_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, Record)>> {
    match driver {
        TableDriver::Empty => Ok(Vec::new()),
        TableDriver::Id(id) => {
            if id.is_empty() || after_id.is_some() {
                return Ok(Vec::new());
            }
            Ok(db
                .get_at_unbudgeted(snapshot, &table.schema.name, id)?
                .map(|record| vec![(id.clone(), record)])
                .unwrap_or_default())
        }
        TableDriver::Equality(column, value) => {
            db.find_eq_batch_unbudgeted(&table.schema.name, column, value, after_id, limit)
        }
        TableDriver::Scan => {
            db.scan_batch_at_unbudgeted(snapshot, &table.schema.name, after_id, limit)
        }
    }
}

/// The common single-table SELECT path is fully batched. It retains only one
/// scan batch plus the caller-owned output; ORDER BY uses bounded sorted runs.
fn exec_single_table_select(
    db: &Db,
    tables: &[TableCtx],
    stmt: &SelectStmt,
    pushed: &[RExpr],
    residual: &[RExpr],
) -> Result<QueryOutput> {
    let (columns, extract) = projection_plan(tables, &stmt.projection)?;
    let offset = limit_to_usize(stmt.offset.as_ref()).unwrap_or(0);
    let limit = limit_to_usize(stmt.limit.as_ref());
    if limit == Some(0) {
        return Ok(QueryOutput::Rows {
            columns,
            rows: Vec::new(),
        });
    }

    let order_keys: Vec<((usize, String), SortSpec)> = stmt
        .order_by
        .iter()
        .map(|key| {
            Ok((
                resolve_col(tables, &key.column)?,
                SortSpec {
                    desc: key.desc,
                    collation: key.collation,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let mut sorter = if order_keys.is_empty() {
        None
    } else {
        Some(SpillSorter::new(
            db,
            order_keys.iter().map(|(_, spec)| *spec).collect(),
            limit.map(|n| offset.saturating_add(n)),
        )?)
    };
    let memory = db.memory_options();
    // Keep decoded scan batches proportional to the byte budget even when the
    // configured row cap was chosen for small records.
    let batch_rows = memory
        .scan_batch_rows
        .min((memory.query_working_bytes / 1024).max(1));
    let snapshot = db.snapshot();
    let driver = table_driver(&tables[0], 0, pushed);
    let mut cursor: Option<String> = None;
    let mut sequence = 0u64;
    let mut skipped = 0usize;
    let mut out = Vec::new();

    loop {
        let batch = driven_batch(
            db,
            &snapshot,
            &tables[0],
            &driver,
            cursor.as_deref(),
            batch_rows,
        )?;
        let Some(last_id) = batch.last().map(|(id, _)| id.clone()) else {
            break;
        };
        cursor = Some(last_id);
        for (_, record) in batch {
            let row = vec![Some(record)];
            if !eval_all(&row, pushed)? || !eval_all(&row, residual)? {
                continue;
            }
            if let Some(sorter) = sorter.as_mut() {
                let keys = order_keys
                    .iter()
                    .map(|((ti, col), _)| col_value(&row, *ti, col))
                    .collect();
                sorter.push(SortedOutputRow {
                    keys,
                    values: project_row(&row, &extract),
                    sequence,
                })?;
                sequence = sequence.saturating_add(1);
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            out.push(project_row(&row, &extract));
            if limit.is_some_and(|n| out.len() == n) {
                return Ok(QueryOutput::Rows { columns, rows: out });
            }
        }
    }

    let rows = match sorter {
        Some(sorter) => sorter.finish(offset, limit)?,
        None => out,
    };
    Ok(QueryOutput::Rows { columns, rows })
}

fn visit_single_table_rows(
    db: &Db,
    table: &TableCtx,
    pushed: &[RExpr],
    residual: &[RExpr],
    mut visit: impl FnMut(&ExecRow) -> Result<()>,
) -> Result<()> {
    let memory = db.memory_options();
    let batch_rows = memory
        .scan_batch_rows
        .min((memory.query_working_bytes / 1024).max(1));
    let snapshot = db.snapshot();
    let driver = table_driver(table, 0, pushed);
    let mut cursor: Option<String> = None;
    loop {
        let batch = driven_batch(db, &snapshot, table, &driver, cursor.as_deref(), batch_rows)?;
        let Some(last_id) = batch.last().map(|(id, _)| id.clone()) else {
            return Ok(());
        };
        cursor = Some(last_id);
        for (_, record) in batch {
            let row = vec![Some(record)];
            if eval_all(&row, pushed)? && eval_all(&row, residual)? {
                visit(&row)?;
            }
        }
    }
}

fn visit_table_records(
    db: &Db,
    table: &TableCtx,
    ti: usize,
    predicates: &[RExpr],
    mut visit: impl FnMut(Record) -> Result<()>,
) -> Result<()> {
    let memory = db.memory_options();
    let batch_rows = memory
        .scan_batch_rows
        .min((memory.query_working_bytes / 1024).max(1));
    let snapshot = db.snapshot();
    let driver = table_driver(table, ti, predicates);
    let mut cursor: Option<String> = None;
    loop {
        let batch = driven_batch(db, &snapshot, table, &driver, cursor.as_deref(), batch_rows)?;
        let Some(last_id) = batch.last().map(|(id, _)| id.clone()) else {
            return Ok(());
        };
        cursor = Some(last_id);
        for (_, record) in batch {
            let row = single_row(ti, record);
            if eval_all(&row, predicates)? {
                visit(take_single(row, ti))?;
            }
        }
    }
}

/// Bounded common JOIN path: stream the left side and probe an id/secondary
/// index on the right. No joined `ExecRow` collection is retained.
fn exec_single_indexed_join_select(
    db: &Db,
    tables: &[TableCtx],
    stmt: &SelectStmt,
    pushdown: &[Vec<RExpr>],
    residual: &[RExpr],
) -> Result<Option<QueryOutput>> {
    let join = &stmt.joins[0];
    let left = resolve_col(tables, &join.on.0)?;
    let right = resolve_col(tables, &join.on.1)?;
    let (existing, fresh) = if left.0 == 1 && right.0 == 0 {
        (right, left)
    } else if right.0 == 1 && left.0 == 0 {
        (left, right)
    } else {
        return Err(Error::Sql(
            "ON must join the new table with a previously listed table".into(),
        ));
    };
    if !join_uses_index_loop(join.kind, &tables[1], &fresh.1) {
        return Ok(None);
    }

    let (columns, extract) = projection_plan(tables, &stmt.projection)?;
    let order_keys: Vec<((usize, String), SortSpec)> = stmt
        .order_by
        .iter()
        .map(|key| {
            Ok((
                resolve_col(tables, &key.column)?,
                SortSpec {
                    desc: key.desc,
                    collation: key.collation,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let offset = limit_to_usize(stmt.offset.as_ref()).unwrap_or(0);
    let limit = limit_to_usize(stmt.limit.as_ref());
    if limit == Some(0) {
        return Ok(Some(QueryOutput::Rows {
            columns,
            rows: Vec::new(),
        }));
    }
    let mut sorter = if order_keys.is_empty() {
        None
    } else {
        Some(SpillSorter::new(
            db,
            order_keys.iter().map(|(_, spec)| *spec).collect(),
            limit.map(|value| offset.saturating_add(value)),
        )?)
    };
    let memory = db.memory_options();
    let batch_rows = memory
        .scan_batch_rows
        .min((memory.query_working_bytes / 1024).max(1));
    let snapshot = db.snapshot();
    let left_driver = table_driver(&tables[0], 0, &pushdown[0]);
    let mut left_cursor: Option<String> = None;
    let mut output = Vec::new();
    let mut skipped = 0usize;
    let mut sequence = 0u64;
    let mut complete = false;

    while !complete {
        let batch = driven_batch(
            db,
            &snapshot,
            &tables[0],
            &left_driver,
            left_cursor.as_deref(),
            batch_rows,
        )?;
        let Some(last_id) = batch.last().map(|(id, _)| id.clone()) else {
            break;
        };
        left_cursor = Some(last_id);
        for (_, left_record) in batch {
            let left_row = vec![Some(left_record)];
            if !eval_all(&left_row, &pushdown[0])? {
                continue;
            }
            let key = col_value(&left_row, existing.0, &existing.1);
            let mut matched = false;
            let mut right_cursor: Option<String> = None;
            loop {
                let matches = if key.is_null() {
                    Vec::new()
                } else if fresh.1 == ID_COLUMN && tables[1].schema.has_implicit_id() {
                    if right_cursor.is_some() {
                        Vec::new()
                    } else if let Value::Text(id) = &key {
                        db.get_at_unbudgeted(&snapshot, &tables[1].schema.name, id)?
                            .map(|record| vec![(id.clone(), record)])
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                } else {
                    let ty = tables[1]
                        .schema
                        .column(&fresh.1)
                        .expect("resolved column")
                        .ty;
                    match coerce_for_lookup(&key, ty) {
                        Some(value) => db.find_eq_batch_unbudgeted(
                            &tables[1].schema.name,
                            &fresh.1,
                            &value,
                            right_cursor.as_deref(),
                            batch_rows,
                        )?,
                        None => Vec::new(),
                    }
                };
                let Some(last_right_id) = matches.last().map(|(id, _)| id.clone()) else {
                    break;
                };
                right_cursor = Some(last_right_id);
                for (_, right_record) in matches {
                    let right_row = vec![None, Some(right_record)];
                    if !eval_all(&right_row, &pushdown[1])? {
                        continue;
                    }
                    matched = true;
                    let row = vec![left_row[0].clone(), right_row[1].clone()];
                    if !eval_all(&row, residual)? {
                        continue;
                    }
                    if let Some(sorter) = sorter.as_mut() {
                        sorter.push(SortedOutputRow {
                            keys: order_keys
                                .iter()
                                .map(|((ti, column), _)| col_value(&row, *ti, column))
                                .collect(),
                            values: project_row(&row, &extract),
                            sequence,
                        })?;
                        sequence = sequence.saturating_add(1);
                    } else if skipped < offset {
                        skipped += 1;
                    } else {
                        output.push(project_row(&row, &extract));
                        if limit.is_some_and(|value| output.len() == value) {
                            complete = true;
                            break;
                        }
                    }
                }
                if complete || (fresh.1 == ID_COLUMN && tables[1].schema.has_implicit_id()) {
                    break;
                }
            }
            if complete {
                break;
            }
            if !matched && join.kind == JoinKind::Left {
                let row = vec![left_row[0].clone(), None];
                if !eval_all(&row, residual)? {
                    continue;
                }
                if let Some(sorter) = sorter.as_mut() {
                    sorter.push(SortedOutputRow {
                        keys: order_keys
                            .iter()
                            .map(|((ti, column), _)| col_value(&row, *ti, column))
                            .collect(),
                        values: project_row(&row, &extract),
                        sequence,
                    })?;
                    sequence = sequence.saturating_add(1);
                } else if skipped < offset {
                    skipped += 1;
                } else {
                    output.push(project_row(&row, &extract));
                    if limit.is_some_and(|value| output.len() == value) {
                        complete = true;
                        break;
                    }
                }
            }
        }
    }

    if let Some(sorter) = sorter {
        output = sorter.finish(offset, limit)?;
    }
    Ok(Some(QueryOutput::Rows {
        columns,
        rows: output,
    }))
}

// --- SELECT -----------------------------------------------------------------------

/// FROM/JOIN resolution plus the WHERE split into per-table pushdown and
/// cross-table residual conjuncts. Shared by execution and `EXPLAIN`.
struct ResolvedQuery {
    tables: Vec<TableCtx>,
    pushdown: Vec<Vec<RExpr>>,
    residual: Vec<RExpr>,
    is_aggregate: bool,
}

fn resolve_query(db: &Db, stmt: &SelectStmt) -> Result<ResolvedQuery> {
    // Resolve FROM + JOIN tables.
    let mut tables: Vec<TableCtx> = Vec::new();
    let mut load = |tref: &TableRef| -> Result<()> {
        let schema = db
            .table_schema(&tref.name)
            .ok_or_else(|| Error::TableNotFound(tref.name.clone()))?;
        let label = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
        if tables
            .iter()
            .any(|t: &TableCtx| t.label.eq_ignore_ascii_case(&label))
        {
            return Err(Error::Sql(format!("duplicate table alias '{label}'")));
        }
        tables.push(TableCtx { schema, label });
        Ok(())
    };
    load(&stmt.from)?;
    for j in &stmt.joins {
        load(&j.table)?;
    }

    // Resolve WHERE into pushdown + residual conjuncts.
    let mut pushdown: Vec<Vec<RExpr>> = (0..tables.len()).map(|_| Vec::new()).collect();
    let mut residual: Vec<RExpr> = Vec::new();
    if let Some(w) = &stmt.where_clause {
        let resolved = resolve_expr(&tables, w)?;
        let mut conjuncts = Vec::new();
        collect_conjuncts(resolved, &mut conjuncts);
        for c in conjuncts {
            match single_table_of(&c) {
                Some(ti) => pushdown[ti].push(c),
                None => residual.push(c),
            }
        }
    }

    let is_aggregate = stmt
        .projection
        .iter()
        .any(|i| matches!(i, SelectItem::Aggregate { .. }))
        || !stmt.group_by.is_empty()
        || stmt.having.is_some();

    Ok(ResolvedQuery {
        tables,
        pushdown,
        residual,
        is_aggregate,
    })
}

fn exec_select(db: &Db, stmt: &SelectStmt) -> Result<QueryOutput> {
    let ResolvedQuery {
        tables,
        pushdown,
        residual,
        is_aggregate,
    } = resolve_query(db, stmt)?;

    if tables.len() == 1 {
        if is_aggregate {
            return exec_single_table_aggregate(db, &tables, stmt, &pushdown[0], &residual);
        }
        return exec_single_table_select(db, &tables, stmt, &pushdown[0], &residual);
    }
    if tables.len() == 2 && !is_aggregate {
        if let Some(output) =
            exec_single_indexed_join_select(db, &tables, stmt, &pushdown, &residual)?
        {
            return Ok(output);
        }
    }

    // Base table.
    let base = fetch_table(db, &tables[0], 0, &pushdown[0])?;
    let mut rows: Vec<ExecRow> = base.into_iter().map(|r| vec![Some(r)]).collect();

    // Joins, left to right.
    for (jn, join) in stmt.joins.iter().enumerate() {
        let new_ti = jn + 1;
        let l = resolve_col(&tables, &join.on.0)?;
        let r = resolve_col(&tables, &join.on.1)?;
        let (existing, fresh) = if l.0 == new_ti && r.0 < new_ti {
            (r, l)
        } else if r.0 == new_ti && l.0 < new_ti {
            (l, r)
        } else {
            return Err(Error::Sql(
                "ON must join the new table with a previously listed table".into(),
            ));
        };
        rows = exec_join(
            db,
            rows,
            &tables[new_ti],
            new_ti,
            existing,
            &fresh.1,
            join.kind,
            &pushdown[new_ti],
        )?;
    }

    // Residual predicate.
    if !residual.is_empty() {
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if eval_all(&row, &residual)? {
                kept.push(row);
            }
        }
        rows = kept;
    }

    // Aggregate queries take their own path from here (grouping, HAVING,
    // ORDER BY over output names, LIMIT, projection).
    if is_aggregate {
        return exec_aggregate(&tables, rows, stmt);
    }

    let offset = limit_to_usize(stmt.offset.as_ref()).unwrap_or(0);
    let limit = limit_to_usize(stmt.limit.as_ref());
    let (columns, extract) = projection_plan(&tables, &stmt.projection)?;
    let order_keys: Vec<((usize, String), SortSpec)> = stmt
        .order_by
        .iter()
        .map(|key| {
            Ok((
                resolve_col(&tables, &key.column)?,
                SortSpec {
                    desc: key.desc,
                    collation: key.collation,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let out_rows = if order_keys.is_empty() {
        rows.into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .map(|row| project_row(&row, &extract))
            .collect()
    } else {
        let mut sorter = SpillSorter::new(
            db,
            order_keys.iter().map(|(_, spec)| *spec).collect(),
            limit.map(|value| offset.saturating_add(value)),
        )?;
        for (sequence, row) in rows.into_iter().enumerate() {
            sorter.push(SortedOutputRow {
                keys: order_keys
                    .iter()
                    .map(|((ti, column), _)| col_value(&row, *ti, column))
                    .collect(),
                values: project_row(&row, &extract),
                sequence: sequence as u64,
            })?;
        }
        sorter.finish(offset, limit)?
    };
    Ok(QueryOutput::Rows {
        columns,
        rows: out_rows,
    })
}

// --- EXPLAIN ------------------------------------------------------------------
//
// EXPLAIN re-derives the plan from the same functions the executor obeys
// (`resolve_query`, `table_driver`, `join_uses_index_loop`, `aggregate_plan`)
// and never runs the query. Planning carries no estimates, so every line is a
// statement about what will happen, not a prediction.

/// Long text values are elided: a plan line must stay readable in a terminal.
const EXPLAIN_TEXT_LIMIT: usize = 32;

fn explain_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(v) => v.to_string(),
        Value::Int64(v) => v.to_string(),
        Value::Float64(v) => v.to_string(),
        Value::Text(v) if v.chars().count() > EXPLAIN_TEXT_LIMIT => {
            let head: String = v.chars().take(EXPLAIN_TEXT_LIMIT).collect();
            format!("'{head}...'")
        }
        Value::Text(v) => format!("'{v}'"),
        Value::Blob(v) => format!("blob[{} bytes]", v.len()),
        Value::Timestamp(v) => format!("timestamp({v})"),
        Value::Date(v) => format!("date({v})"),
        Value::Time(v) => format!("time({v})"),
        Value::Json(_) => "json".into(),
        Value::Vector(v) => format!("vector[{}]", v.len()),
    }
}

fn explain_col(tables: &[TableCtx], col: &(usize, String)) -> String {
    format!("{}.{}", tables[col.0].label, col.1)
}

fn explain_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Neq => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

fn explain_rval(tables: &[TableCtx], aggs: &[String], value: &RVal) -> String {
    match value {
        RVal::Col(ti, column) => format!("{}.{column}", tables[*ti].label),
        RVal::Val(v) => explain_value(v),
        RVal::Agg(index) => aggs
            .get(*index)
            .cloned()
            .unwrap_or_else(|| format!("agg#{index}")),
    }
}

fn explain_expr(tables: &[TableCtx], aggs: &[String], expr: &RExpr) -> String {
    match expr {
        RExpr::Cmp { left, op, right } => format!(
            "{} {} {}",
            explain_rval(tables, aggs, left),
            explain_op(*op),
            explain_rval(tables, aggs, right)
        ),
        RExpr::IsNull { col, negated } => format!(
            "{} IS {}NULL",
            explain_col(tables, col),
            if *negated { "NOT " } else { "" }
        ),
        RExpr::InList { col, list, negated } => format!(
            "{} {}IN ({})",
            explain_col(tables, col),
            if *negated { "NOT " } else { "" },
            list.iter()
                .map(explain_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        RExpr::And(a, b) => format!(
            "({} AND {})",
            explain_expr(tables, aggs, a),
            explain_expr(tables, aggs, b)
        ),
        RExpr::Or(a, b) => format!(
            "({} OR {})",
            explain_expr(tables, aggs, a),
            explain_expr(tables, aggs, b)
        ),
        RExpr::Not(inner) => format!("NOT {}", explain_expr(tables, aggs, inner)),
    }
}

/// Accumulates plan lines; `depth` is rendered as indentation so the result is
/// a single text column that reads as a tree.
struct PlanBuilder {
    rows: Vec<Vec<Value>>,
}

impl PlanBuilder {
    fn new() -> Self {
        PlanBuilder { rows: Vec::new() }
    }

    fn line(&mut self, depth: usize, text: impl AsRef<str>) {
        let mut out = "  ".repeat(depth);
        out.push_str(text.as_ref());
        self.rows.push(vec![Value::Text(out)]);
    }

    fn filters(&mut self, depth: usize, tables: &[TableCtx], conjuncts: &[RExpr]) {
        for conjunct in conjuncts {
            self.line(
                depth,
                format!("filter: {}", explain_expr(tables, &[], conjunct)),
            );
        }
    }

    fn finish(self) -> QueryOutput {
        QueryOutput::Rows {
            columns: vec!["plan".into()],
            rows: self.rows,
        }
    }
}

/// The access path `table_driver` picked, named after the mechanism the storage
/// layer will actually use.
fn explain_access(
    plan: &mut PlanBuilder,
    depth: usize,
    tables: &[TableCtx],
    ti: usize,
    pushed: &[RExpr],
) {
    let table = &tables[ti];
    let label = &table.label;
    let (driver, driving) = table_driver_at(table, ti, pushed);
    let empty = matches!(driver, TableDriver::Empty);
    let line = match driver {
        TableDriver::Empty => {
            format!("NO ACCESS {label}  (equality on NULL matches no row)")
        }
        TableDriver::Id(id) => format!("POINT LOOKUP {label}.id = '{id}'"),
        TableDriver::Equality(column, value) if has_secondary_index(table, &column) => {
            format!("INDEX LOOKUP {label}.{column} = {}", explain_value(&value))
        }
        // Without a secondary index find_eq walks the primary directory and
        // filters, which costs a full scan however selective the predicate is.
        TableDriver::Equality(column, value) => format!(
            "SCAN {label}  (equality {column} = {}, no index)",
            explain_value(&value)
        ),
        TableDriver::Scan => format!("SCAN {label}"),
    };
    plan.line(depth, line);
    // Nothing is read on the empty path, so nothing is filtered either.
    if empty {
        return;
    }
    let remaining: Vec<RExpr> = pushed
        .iter()
        .enumerate()
        .filter(|(position, _)| Some(*position) != driving)
        .map(|(_, predicate)| predicate.clone())
        .collect();
    plan.filters(depth + 1, tables, &remaining);
}

/// Emits the left-deep join tree with the outermost (last) join at the root.
/// `joins` is how many of `stmt.joins` this subtree covers.
fn explain_join_tree(
    plan: &mut PlanBuilder,
    depth: usize,
    stmt: &SelectStmt,
    tables: &[TableCtx],
    pushdown: &[Vec<RExpr>],
    joins: usize,
    streamed: bool,
) -> Result<()> {
    let Some(index) = joins.checked_sub(1) else {
        explain_access(plan, depth, tables, 0, &pushdown[0]);
        return Ok(());
    };
    let join = &stmt.joins[index];
    let new_ti = index + 1;
    let left = resolve_col(tables, &join.on.0)?;
    let right = resolve_col(tables, &join.on.1)?;
    let (existing, fresh) = if left.0 == new_ti && right.0 < new_ti {
        (right, left)
    } else if right.0 == new_ti && left.0 < new_ti {
        (left, right)
    } else {
        return Err(Error::Sql(
            "ON must join the new table with a previously listed table".into(),
        ));
    };
    let kind = match join.kind {
        JoinKind::Inner => "INNER",
        JoinKind::Left => "LEFT",
        JoinKind::Right => "RIGHT",
    };
    let index_loop = join_uses_index_loop(join.kind, &tables[new_ti], &fresh.1);
    let strategy = if index_loop {
        "index nested-loop"
    } else {
        "grace hash join"
    };
    plan.line(depth, format!("JOIN {kind} ({strategy})"));
    plan.line(
        depth + 1,
        format!(
            "on: {} = {}",
            explain_col(tables, &existing),
            explain_col(tables, &fresh)
        ),
    );
    if index_loop && streamed {
        plan.line(depth + 1, "streamed: no joined rows are materialized");
    }
    explain_join_tree(plan, depth + 1, stmt, tables, pushdown, index, streamed)?;
    if index_loop {
        let probe = if fresh.1 == ID_COLUMN && tables[new_ti].schema.has_implicit_id() {
            format!(
                "POINT LOOKUP {}.id = {}",
                tables[new_ti].label,
                explain_col(tables, &existing)
            )
        } else {
            format!(
                "INDEX PROBE {} = {}",
                explain_col(tables, &fresh),
                explain_col(tables, &existing)
            )
        };
        plan.line(depth + 1, probe);
        plan.filters(depth + 2, tables, &pushdown[new_ti]);
    } else {
        explain_access(plan, depth + 1, tables, new_ti, &pushdown[new_ti]);
    }
    Ok(())
}

fn explain_select(db: &Db, stmt: &SelectStmt) -> Result<QueryOutput> {
    let ResolvedQuery {
        tables,
        pushdown,
        residual,
        is_aggregate,
    } = resolve_query(db, stmt)?;

    // Build the same plans execution builds, so EXPLAIN rejects exactly the
    // queries that cannot run instead of printing a plan for one of them.
    let aggregate = if is_aggregate {
        let plan = aggregate_plan(&tables, stmt)?;
        aggregate_order_positions(stmt, &plan.headers)?;
        Some(plan)
    } else {
        projection_plan(&tables, &stmt.projection)?;
        for key in &stmt.order_by {
            resolve_col(&tables, &key.column)?;
        }
        None
    };
    let agg_names: Vec<String> = match &aggregate {
        Some(plan) => plan
            .specs
            .iter()
            .map(|spec| match &spec.arg {
                Some(col) => format!("{}({})", spec.func.name(), explain_col(&tables, col)),
                None => format!("{}(*)", spec.func.name()),
            })
            .collect(),
        None => Vec::new(),
    };

    let mut plan = PlanBuilder::new();
    let mut depth = 0;

    // Outermost operators first: LIMIT wraps the sort, which wraps grouping.
    if stmt.limit.is_some() || stmt.offset.is_some() {
        let mut line = String::from("LIMIT");
        match limit_to_usize(stmt.limit.as_ref()) {
            Some(n) => line.push_str(&format!(" {n}")),
            None => line.push_str(" ALL"),
        }
        if let Some(offset) = limit_to_usize(stmt.offset.as_ref()) {
            line.push_str(&format!(" OFFSET {offset}"));
        }
        plan.line(depth, line);
        depth += 1;
    }
    if !stmt.order_by.is_empty() {
        let keys: Vec<String> = stmt
            .order_by
            .iter()
            .map(|key| {
                let name = match aggregate {
                    // Aggregate ORDER BY addresses output columns by name.
                    Some(_) => key.column.column.clone(),
                    None => explain_col(&tables, &resolve_col(&tables, &key.column)?),
                };
                let direction = if key.desc { "DESC" } else { "ASC" };
                // Only name the collation when it is not the default, so the
                // common plan stays quiet.
                let collation = if key.collation == Collation::default() {
                    String::new()
                } else {
                    format!(" COLLATE {}", key.collation.name())
                };
                Ok(format!("{name} {direction}{collation}"))
            })
            .collect::<Result<_>>()?;
        plan.line(depth, format!("SORT {}", keys.join(", ")));
        plan.line(
            depth + 1,
            "external merge sort, spills to disk over the query budget",
        );
        depth += 1;
    }
    if let Some(aggregate) = &aggregate {
        let line = if aggregate.group_cols.is_empty() {
            "AGGREGATE (single group)".to_string()
        } else {
            format!(
                "GROUP BY {}",
                aggregate
                    .group_cols
                    .iter()
                    .map(|col| explain_col(&tables, col))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        plan.line(depth, line);
        if !agg_names.is_empty() {
            plan.line(depth + 1, format!("aggregates: {}", agg_names.join(", ")));
        }
        if let Some(having) = &aggregate.having {
            plan.line(
                depth + 1,
                format!("having: {}", explain_expr(&tables, &agg_names, having)),
            );
        }
        depth += 1;
    }

    // Residual conjuncts span tables, so they are evaluated above the join.
    for conjunct in &residual {
        plan.line(
            depth,
            format!("filter: {}", explain_expr(&tables, &agg_names, conjunct)),
        );
    }

    // The bounded streaming join path only handles a two-table, non-aggregate
    // SELECT; see exec_select's dispatch.
    let streamed = tables.len() == 2 && !is_aggregate;
    explain_join_tree(
        &mut plan,
        depth,
        stmt,
        &tables,
        &pushdown,
        stmt.joins.len(),
        streamed,
    )?;
    Ok(plan.finish())
}

// --- aggregation --------------------------------------------------------------

#[derive(PartialEq)]
struct AggSpec {
    func: AggFunc,
    arg: Option<(usize, String)>,
    distinct: bool,
}

enum AggState {
    Count(u64),
    CountDistinct(HashSet<Vec<u8>>),
    Sum {
        ints: i128,
        floats: f64,
        saw_float: bool,
        saw_any: bool,
    },
    Avg {
        sum: f64,
        n: u64,
    },
    MinMax {
        best: Option<Value>,
        is_min: bool,
    },
}

impl AggState {
    fn new(spec: &AggSpec) -> AggState {
        match spec.func {
            AggFunc::Count if spec.distinct => AggState::CountDistinct(HashSet::new()),
            AggFunc::Count => AggState::Count(0),
            AggFunc::Sum => AggState::Sum {
                ints: 0,
                floats: 0.0,
                saw_float: false,
                saw_any: false,
            },
            AggFunc::Avg => AggState::Avg { sum: 0.0, n: 0 },
            AggFunc::Min => AggState::MinMax {
                best: None,
                is_min: true,
            },
            AggFunc::Max => AggState::MinMax {
                best: None,
                is_min: false,
            },
        }
    }

    /// SQL NULL semantics: COUNT(col)/SUM/AVG/MIN/MAX ignore NULLs;
    /// COUNT(*) counts rows.
    fn update(&mut self, spec: &AggSpec, row: &ExecRow) -> Result<()> {
        let value = spec
            .arg
            .as_ref()
            .map(|(ti, col)| col_value(row, *ti, col))
            .unwrap_or(Value::Null);
        self.update_value(spec, &value)
    }

    fn update_value(&mut self, spec: &AggSpec, value: &Value) -> Result<()> {
        match self {
            AggState::Count(n) => match &spec.arg {
                None => *n += 1,
                Some((ti, col)) => {
                    let _ = (ti, col);
                    if !value.is_null() {
                        *n += 1;
                    }
                }
            },
            AggState::CountDistinct(values) => {
                if !value.is_null() {
                    let mut encoded = Vec::new();
                    encode_value(&mut encoded, value);
                    values.insert(encoded);
                }
            }
            AggState::Sum {
                ints,
                floats,
                saw_float,
                saw_any,
            } => match value {
                Value::Null => {}
                Value::Int64(x) => {
                    *ints += *x as i128;
                    *saw_any = true;
                }
                Value::Float64(f) => {
                    *floats += *f;
                    *saw_float = true;
                    *saw_any = true;
                }
                other => return Err(Error::Sql(format!("SUM over non-numeric value {other:?}"))),
            },
            AggState::Avg { sum, n } => match value {
                Value::Null => {}
                Value::Int64(x) => {
                    *sum += *x as f64;
                    *n += 1;
                }
                Value::Float64(f) => {
                    *sum += *f;
                    *n += 1;
                }
                other => return Err(Error::Sql(format!("AVG over non-numeric value {other:?}"))),
            },
            AggState::MinMax { best, is_min } => {
                if value.is_null() {
                    return Ok(());
                }
                match best {
                    None => *best = Some(value.clone()),
                    Some(b) => {
                        let ord = cmp_vals(b, value).ok_or_else(|| not_comparable(b, value))?;
                        let replace = if *is_min {
                            ord == Ordering::Greater
                        } else {
                            ord == Ordering::Less
                        };
                        if replace {
                            *best = Some(value.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Value> {
        Ok(match self {
            AggState::Count(n) => Value::Int64(n as i64),
            AggState::CountDistinct(values) => Value::Int64(values.len() as i64),
            AggState::Sum {
                ints,
                floats,
                saw_float,
                saw_any,
            } => {
                if !saw_any {
                    Value::Null
                } else if saw_float {
                    Value::Float64(floats + ints as f64)
                } else {
                    Value::Int64(
                        i64::try_from(ints)
                            .map_err(|_| Error::Sql("SUM overflows int64".into()))?,
                    )
                }
            }
            AggState::Avg { sum, n } => {
                if n == 0 {
                    Value::Null
                } else {
                    Value::Float64(sum / n as f64)
                }
            }
            AggState::MinMax { best, .. } => best.unwrap_or(Value::Null),
        })
    }
}

fn agg_index(aggs: &mut Vec<AggSpec>, spec: AggSpec) -> usize {
    match aggs.iter().position(|s| *s == spec) {
        Some(i) => i,
        None => {
            aggs.push(spec);
            aggs.len() - 1
        }
    }
}

fn col_type(tables: &[TableCtx], ti: usize, col: &str) -> ColumnType {
    if col == ID_COLUMN && tables[ti].schema.has_implicit_id() {
        ColumnType::Text
    } else {
        tables[ti].schema.column(col).expect("resolved").ty
    }
}

fn validate_agg(tables: &[TableCtx], func: AggFunc, arg: &Option<(usize, String)>) -> Result<()> {
    if matches!(func, AggFunc::Sum | AggFunc::Avg) {
        let (ti, col) = arg.as_ref().expect("parser guarantees an argument");
        let ty = col_type(tables, *ti, col);
        if !matches!(ty, ColumnType::Int64 | ColumnType::Float64) {
            return Err(Error::Sql(format!(
                "{}({col}) requires an int64 or float64 column, got {ty}",
                func.name()
            )));
        }
    }
    Ok(())
}

fn resolve_having_expr(
    tables: &[TableCtx],
    expr: &Expr,
    group_set: &HashSet<(usize, String)>,
    aggs: &mut Vec<AggSpec>,
) -> Result<RExpr> {
    let grouped_col = |tables: &[TableCtx], c: &ColumnRef| -> Result<(usize, String)> {
        let rc = resolve_col(tables, c)?;
        if !group_set.contains(&rc) {
            return Err(Error::Sql(format!(
                "HAVING can only reference grouped columns and aggregates; '{}' is not grouped",
                c.column
            )));
        }
        Ok(rc)
    };
    Ok(match expr {
        Expr::Cmp { left, op, right } => RExpr::Cmp {
            left: resolve_having_operand(tables, left, group_set, aggs)?,
            op: *op,
            right: resolve_having_operand(tables, right, group_set, aggs)?,
        },
        Expr::IsNull { col, negated } => RExpr::IsNull {
            col: grouped_col(tables, col)?,
            negated: *negated,
        },
        Expr::InList { col, list, negated } => RExpr::InList {
            col: grouped_col(tables, col)?,
            list: list.iter().map(literal_to_plain_value).collect(),
            negated: *negated,
        },
        Expr::And(a, b) => RExpr::And(
            Box::new(resolve_having_expr(tables, a, group_set, aggs)?),
            Box::new(resolve_having_expr(tables, b, group_set, aggs)?),
        ),
        Expr::Or(a, b) => RExpr::Or(
            Box::new(resolve_having_expr(tables, a, group_set, aggs)?),
            Box::new(resolve_having_expr(tables, b, group_set, aggs)?),
        ),
        Expr::Not(e) => RExpr::Not(Box::new(resolve_having_expr(tables, e, group_set, aggs)?)),
    })
}

fn resolve_having_operand(
    tables: &[TableCtx],
    op: &Operand,
    group_set: &HashSet<(usize, String)>,
    aggs: &mut Vec<AggSpec>,
) -> Result<RVal> {
    Ok(match op {
        Operand::Col(c) => {
            let rc = resolve_col(tables, c)?;
            if !group_set.contains(&rc) {
                return Err(Error::Sql(format!(
                    "HAVING can only reference grouped columns and aggregates; '{}' is not grouped",
                    c.column
                )));
            }
            RVal::Col(rc.0, rc.1)
        }
        Operand::Lit(l) => RVal::Val(literal_to_plain_value(l)),
        Operand::Agg {
            func,
            arg,
            distinct,
        } => {
            let arg_r = match arg {
                Some(c) => Some(resolve_col(tables, c)?),
                None => None,
            };
            validate_agg(tables, *func, &arg_r)?;
            RVal::Agg(agg_index(
                aggs,
                AggSpec {
                    func: *func,
                    arg: arg_r,
                    distinct: *distinct,
                },
            ))
        }
    })
}

enum OutCol {
    Group(usize),
    Agg(usize),
}

/// Everything an aggregate query needs decided before any row is read: the
/// GROUP BY keys, the validated projection, the deduplicated aggregate specs
/// and the resolved HAVING. Both aggregate executors and `EXPLAIN` build it,
/// so all three agree on which aggregate queries are legal.
struct AggregatePlan {
    group_cols: Vec<(usize, String)>,
    specs: Vec<AggSpec>,
    headers: Vec<String>,
    out_cols: Vec<OutCol>,
    having: Option<RExpr>,
}

fn aggregate_plan(tables: &[TableCtx], stmt: &SelectStmt) -> Result<AggregatePlan> {
    let group_cols: Vec<(usize, String)> = stmt
        .group_by
        .iter()
        .map(|column| resolve_col(tables, column))
        .collect::<Result<_>>()?;
    let group_set: HashSet<(usize, String)> = group_cols.iter().cloned().collect();
    let mut specs = Vec::new();
    let mut headers = Vec::new();
    let mut out_cols = Vec::new();
    for item in &stmt.projection {
        match item {
            SelectItem::Star => {
                return Err(Error::Sql(
                    "SELECT * cannot be combined with aggregates/GROUP BY; list columns explicitly"
                        .into(),
                ));
            }
            SelectItem::Column { col, alias } => {
                let resolved = resolve_col(tables, col)?;
                let Some(group_index) = group_cols.iter().position(|group| *group == resolved)
                else {
                    return Err(Error::Sql(format!(
                        "column '{}' must appear in GROUP BY",
                        col.column
                    )));
                };
                headers.push(alias.clone().unwrap_or_else(|| resolved.1.clone()));
                out_cols.push(OutCol::Group(group_index));
            }
            SelectItem::Aggregate {
                func,
                arg,
                distinct,
                alias,
            } => {
                let argument = match arg {
                    Some(column) => Some(resolve_col(tables, column)?),
                    None => None,
                };
                validate_agg(tables, *func, &argument)?;
                let default = match &argument {
                    Some((_, name)) if *distinct => {
                        format!("{}(distinct {name})", func.name())
                    }
                    Some((_, name)) => format!("{}({name})", func.name()),
                    None => format!("{}(*)", func.name()),
                };
                let index = agg_index(
                    &mut specs,
                    AggSpec {
                        func: *func,
                        arg: argument,
                        distinct: *distinct,
                    },
                );
                headers.push(alias.clone().unwrap_or(default));
                out_cols.push(OutCol::Agg(index));
            }
        }
    }
    let having = match &stmt.having {
        Some(expr) => Some(resolve_having_expr(tables, expr, &group_set, &mut specs)?),
        None => None,
    };
    Ok(AggregatePlan {
        group_cols,
        specs,
        headers,
        out_cols,
        having,
    })
}

/// ORDER BY in an aggregate query addresses output columns, not input ones.
fn aggregate_order_positions(
    stmt: &SelectStmt,
    headers: &[String],
) -> Result<Vec<(usize, SortSpec)>> {
    stmt.order_by
        .iter()
        .map(|key| {
            headers
                .iter()
                .position(|header| header.eq_ignore_ascii_case(&key.column.column))
                .map(|position| {
                    (
                        position,
                        SortSpec {
                            desc: key.desc,
                            collation: key.collation,
                        },
                    )
                })
                .ok_or_else(|| {
                    Error::Sql(format!(
                        "in aggregate queries ORDER BY must reference an output column or alias; '{}' is neither",
                        key.column.column
                    ))
                })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn queue_aggregate_group(
    sorter: &mut SpillSorter<'_>,
    group_cols: &[(usize, String)],
    out_cols: &[OutCol],
    having: Option<&RExpr>,
    order_positions: &[(usize, SortSpec)],
    group_values: Vec<Value>,
    states: Vec<AggState>,
    first_sequence: u64,
) -> Result<()> {
    let aggregate_values: Vec<Value> = states
        .into_iter()
        .map(AggState::finish)
        .collect::<Result<_>>()?;
    if let Some(expr) = having {
        let group_map: HashMap<(usize, String), Value> = group_cols
            .iter()
            .cloned()
            .zip(group_values.iter().cloned())
            .collect();
        if !eval_ctx(&Vec::new(), expr, Some((&group_map, &aggregate_values)))?.is_true() {
            return Ok(());
        }
    }
    let values: Vec<Value> = out_cols
        .iter()
        .map(|column| match column {
            OutCol::Group(index) => group_values[*index].clone(),
            OutCol::Agg(index) => aggregate_values[*index].clone(),
        })
        .collect();
    let keys = if order_positions.is_empty() {
        vec![Value::Int64(
            i64::try_from(first_sequence).unwrap_or(i64::MAX),
        )]
    } else {
        order_positions
            .iter()
            .map(|(position, _)| values[*position].clone())
            .collect()
    };
    sorter.push(SortedOutputRow {
        keys,
        values,
        sequence: first_sequence,
    })
}

/// Sort-based aggregation for the single-table path. Input rows are streamed;
/// GROUP BY keys are externally sorted under the query budget, so cardinality
/// does not translate into an unbounded HashMap.
fn exec_single_table_aggregate(
    db: &Db,
    tables: &[TableCtx],
    stmt: &SelectStmt,
    pushed: &[RExpr],
    residual: &[RExpr],
) -> Result<QueryOutput> {
    let AggregatePlan {
        group_cols,
        specs,
        headers,
        out_cols,
        having,
    } = aggregate_plan(tables, stmt)?;
    let order_positions = aggregate_order_positions(stmt, &headers)?;
    let offset = limit_to_usize(stmt.offset.as_ref()).unwrap_or(0);
    let limit = limit_to_usize(stmt.limit.as_ref());
    let output_specs = if order_positions.is_empty() {
        vec![SortSpec::ascending()]
    } else {
        order_positions.iter().map(|(_, spec)| *spec).collect()
    };
    let mut output_sorter = SpillSorter::new(
        db,
        output_specs,
        limit.map(|value| offset.saturating_add(value)),
    )?;
    let new_states = || specs.iter().map(AggState::new).collect::<Vec<_>>();

    if group_cols.is_empty() {
        let mut states = new_states();
        if specs.iter().any(|spec| spec.distinct) {
            // Global COUNT(DISTINCT) is externally sorted, so cardinality is
            // bounded by the query spill budget instead of a RAM hash set.
            let mut distinct_sorter = SpillSorter::new(db, vec![SortSpec::ascending()], None)?;
            let mut sequence = 0u64;
            visit_single_table_rows(db, &tables[0], pushed, residual, |row| {
                for (index, spec) in specs.iter().enumerate() {
                    if spec.distinct {
                        let (table_index, column) =
                            spec.arg.as_ref().expect("DISTINCT has an argument");
                        let value = col_value(row, *table_index, column);
                        if value.is_null() {
                            continue;
                        }
                        let mut key = (index as u64).to_be_bytes().to_vec();
                        encode_value(&mut key, &value);
                        distinct_sorter.push(SortedOutputRow {
                            keys: vec![Value::Blob(key.clone())],
                            values: vec![Value::Int64(index as i64)],
                            sequence,
                        })?;
                        sequence = sequence.saturating_add(1);
                    } else {
                        states[index].update(spec, row)?;
                    }
                }
                Ok(())
            })?;
            let mut previous: Option<Vec<u8>> = None;
            let mut counts = vec![0u64; specs.len()];
            distinct_sorter.for_each_sorted(0, None, |row| {
                let Value::Blob(key) = &row.keys[0] else {
                    return Err(Error::Corrupt("invalid DISTINCT spill key".into()));
                };
                if previous.as_ref() != Some(key) {
                    let Value::Int64(index) = row.values[0] else {
                        return Err(Error::Corrupt("invalid DISTINCT aggregate index".into()));
                    };
                    counts[index as usize] = counts[index as usize].saturating_add(1);
                    previous = Some(key.clone());
                }
                Ok(())
            })?;
            for (index, spec) in specs.iter().enumerate() {
                if spec.distinct {
                    states[index] = AggState::Count(counts[index]);
                }
            }
        } else {
            visit_single_table_rows(db, &tables[0], pushed, residual, |row| {
                for (index, spec) in specs.iter().enumerate() {
                    states[index].update(spec, row)?;
                }
                Ok(())
            })?;
        }
        queue_aggregate_group(
            &mut output_sorter,
            &group_cols,
            &out_cols,
            having.as_ref(),
            &order_positions,
            Vec::new(),
            states,
            0,
        )?;
    } else {
        let mut input_sorter = SpillSorter::new(db, vec![SortSpec::ascending()], None)?;
        let mut sequence = 0u64;
        visit_single_table_rows(db, &tables[0], pushed, residual, |row| {
            let group_values: Vec<Value> = group_cols
                .iter()
                .map(|(ti, column)| col_value(row, *ti, column))
                .collect();
            let mut encoded_key = Vec::new();
            for value in &group_values {
                encode_value(&mut encoded_key, value);
            }
            let mut values = Vec::with_capacity(1 + group_values.len() + specs.len());
            values.push(Value::Blob(encoded_key.clone()));
            values.extend(group_values);
            values.extend(specs.iter().map(|spec| {
                spec.arg
                    .as_ref()
                    .map(|(ti, column)| col_value(row, *ti, column))
                    .unwrap_or(Value::Null)
            }));
            input_sorter.push(SortedOutputRow {
                keys: vec![Value::Blob(encoded_key)],
                values,
                sequence,
            })?;
            sequence = sequence.saturating_add(1);
            Ok(())
        })?;

        let mut current_key: Option<Vec<u8>> = None;
        let mut current_groups = Vec::new();
        let mut current_states: Option<Vec<AggState>> = None;
        let mut first_sequence = 0u64;
        input_sorter.for_each_sorted(0, None, |row| {
            let mut values = row.values.into_iter();
            let Value::Blob(key) = values.next().expect("group key present") else {
                return Err(Error::Corrupt("invalid aggregate spill key".into()));
            };
            let group_values: Vec<Value> = values.by_ref().take(group_cols.len()).collect();
            let inputs: Vec<Value> = values.collect();
            if current_key.as_ref().is_some_and(|current| current != &key) {
                queue_aggregate_group(
                    &mut output_sorter,
                    &group_cols,
                    &out_cols,
                    having.as_ref(),
                    &order_positions,
                    std::mem::take(&mut current_groups),
                    current_states.take().expect("current aggregate states"),
                    first_sequence,
                )?;
            }
            if current_key.as_ref() != Some(&key) {
                current_key = Some(key);
                current_groups = group_values;
                current_states = Some(new_states());
                first_sequence = row.sequence;
            }
            let states = current_states
                .as_mut()
                .expect("aggregate states initialized");
            for (index, spec) in specs.iter().enumerate() {
                states[index].update_value(spec, &inputs[index])?;
            }
            Ok(())
        })?;
        if current_key.is_some() {
            queue_aggregate_group(
                &mut output_sorter,
                &group_cols,
                &out_cols,
                having.as_ref(),
                &order_positions,
                current_groups,
                current_states.expect("current aggregate states"),
                first_sequence,
            )?;
        }
    }

    Ok(QueryOutput::Rows {
        columns: headers,
        rows: output_sorter.finish(offset, limit)?,
    })
}

/// Hash aggregation with first-seen group order, HAVING over group columns
/// and aggregates, ORDER BY over the output headers, then OFFSET/LIMIT.
fn exec_aggregate(
    tables: &[TableCtx],
    rows: Vec<ExecRow>,
    stmt: &SelectStmt,
) -> Result<QueryOutput> {
    let AggregatePlan {
        group_cols,
        specs,
        headers,
        out_cols,
        having: having_r,
    } = aggregate_plan(tables, stmt)?;

    // Group in first-seen order.
    let new_states =
        |specs: &[AggSpec]| -> Vec<AggState> { specs.iter().map(AggState::new).collect() };
    let mut order: Vec<Vec<u8>> = Vec::new();
    let mut groups: HashMap<Vec<u8>, (Vec<Value>, Vec<AggState>)> = HashMap::new();
    if group_cols.is_empty() {
        // A global aggregate always yields exactly one row, even with no input.
        order.push(Vec::new());
        groups.insert(Vec::new(), (Vec::new(), new_states(&specs)));
    }
    for row in &rows {
        let key_vals: Vec<Value> = group_cols
            .iter()
            .map(|(ti, c)| col_value(row, *ti, c))
            .collect();
        let mut key = Vec::new();
        for v in &key_vals {
            encode_value(&mut key, v);
        }
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            (key_vals, new_states(&specs))
        });
        for (ai, spec) in specs.iter().enumerate() {
            entry.1[ai].update(spec, row)?;
        }
    }

    // Finalize, filter with HAVING, project.
    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for key in &order {
        let (key_vals, states) = groups.remove(key).expect("group present");
        let agg_vals: Vec<Value> = states
            .into_iter()
            .map(|s| s.finish())
            .collect::<Result<_>>()?;
        if let Some(h) = &having_r {
            let group_map: HashMap<(usize, String), Value> = group_cols
                .iter()
                .cloned()
                .zip(key_vals.iter().cloned())
                .collect();
            let empty_row: ExecRow = Vec::new();
            if !eval_ctx(&empty_row, h, Some((&group_map, &agg_vals)))?.is_true() {
                continue;
            }
        }
        out_rows.push(
            out_cols
                .iter()
                .map(|oc| match oc {
                    OutCol::Group(gi) => key_vals[*gi].clone(),
                    OutCol::Agg(ai) => agg_vals[*ai].clone(),
                })
                .collect(),
        );
    }

    // ORDER BY references output headers (column names or aliases).
    if !stmt.order_by.is_empty() {
        let keys: Vec<(usize, SortSpec)> = stmt
            .order_by
            .iter()
            .map(|key| {
                headers
                    .iter()
                    .position(|h| h.eq_ignore_ascii_case(&key.column.column))
                    .map(|pos| {
                        (
                            pos,
                            SortSpec {
                                desc: key.desc,
                                collation: key.collation,
                            },
                        )
                    })
                    .ok_or_else(|| {
                        Error::Sql(format!(
                            "in aggregate queries ORDER BY must reference an output column or alias; '{}' is neither",
                            key.column.column
                        ))
                    })
            })
            .collect::<Result<_>>()?;
        out_rows.sort_by(|a, b| {
            for (i, spec) in &keys {
                let ord = sort_cmp(&a[*i], &b[*i], spec.collation);
                if ord != Ordering::Equal {
                    return if spec.desc { ord.reverse() } else { ord };
                }
            }
            Ordering::Equal
        });
    }

    let offset = limit_to_usize(stmt.offset.as_ref()).unwrap_or(0);
    if offset > 0 {
        out_rows = out_rows.into_iter().skip(offset).collect();
    }
    if let Some(limit) = limit_to_usize(stmt.limit.as_ref()) {
        out_rows.truncate(limit);
    }
    Ok(QueryOutput::Rows {
        columns: headers,
        rows: out_rows,
    })
}

const GRACE_JOIN_PARTITIONS: usize = 16;
const GRACE_JOIN_MAX_DEPTH: usize = 4;

struct JoinPartitionSet {
    left_paths: Vec<PathBuf>,
    right_paths: Vec<PathBuf>,
    left_files: Vec<File>,
    right_files: Vec<File>,
}

struct GraceHashJoin<'a> {
    db: &'a Db,
    budget: usize,
    spill_dir: PathBuf,
    files: SpillFiles,
    new_ti: usize,
    existing: (usize, String),
    new_col: &'a str,
    kind: JoinKind,
    out: Vec<ExecRow>,
}

impl<'a> GraceHashJoin<'a> {
    fn new(
        db: &'a Db,
        new_ti: usize,
        existing: (usize, String),
        new_col: &'a str,
        kind: JoinKind,
    ) -> Self {
        let memory = db.memory_options();
        Self {
            db,
            budget: memory.query_working_bytes,
            spill_dir: memory
                .spill_directory
                .unwrap_or_else(|| std::env::temp_dir().join("elitesql-query-spill")),
            files: SpillFiles(Vec::new()),
            new_ti,
            existing,
            new_col,
            kind,
            out: Vec::new(),
        }
    }

    fn execute(
        mut self,
        left_rows: Vec<ExecRow>,
        new_table: &TableCtx,
        pushdown: &[RExpr],
    ) -> Result<Vec<ExecRow>> {
        fs::create_dir_all(&self.spill_dir)?;
        let mut partitions = self.new_partition_set("root")?;
        let mut left_bytes = vec![0u64; GRACE_JOIN_PARTITIONS];
        for row in left_rows {
            self.db.record_query_buffer(exec_row_heap_bytes(&row));
            let key = join_key(&col_value(&row, self.existing.0, &self.existing.1));
            let partition = grace_partition(key.as_deref(), 0);
            left_bytes[partition] +=
                write_spill_exec_row(&mut partitions.left_files[partition], &row)?;
        }
        let mut right_bytes = vec![0u64; GRACE_JOIN_PARTITIONS];
        visit_table_records(self.db, new_table, self.new_ti, pushdown, |record| {
            self.db.record_query_buffer(record_heap_bytes(&record));
            let key = record.get(self.new_col).and_then(join_key);
            let partition = grace_partition(key.as_deref(), 0);
            right_bytes[partition] +=
                write_spill_record(&mut partitions.right_files[partition], &record)?;
            Ok(())
        })?;
        drop(partitions.left_files);
        drop(partitions.right_files);
        self.record_partition_stats(&left_bytes);
        self.record_partition_stats(&right_bytes);

        for (partition, &bytes) in right_bytes.iter().enumerate() {
            self.process_partition(
                &partitions.left_paths[partition],
                &partitions.right_paths[partition],
                0,
                bytes,
            )?;
        }
        Ok(self.out)
    }

    fn new_partition_set(&mut self, tag: &str) -> Result<JoinPartitionSet> {
        let mut left_paths = Vec::with_capacity(GRACE_JOIN_PARTITIONS);
        let mut right_paths = Vec::with_capacity(GRACE_JOIN_PARTITIONS);
        let mut left_files = Vec::with_capacity(GRACE_JOIN_PARTITIONS);
        let mut right_files = Vec::with_capacity(GRACE_JOIN_PARTITIONS);
        let id = Ulid::new();
        for partition in 0..GRACE_JOIN_PARTITIONS {
            let left = self
                .spill_dir
                .join(format!("hash-{id}-{tag}-l-{partition:02}.run"));
            let right = self
                .spill_dir
                .join(format!("hash-{id}-{tag}-r-{partition:02}.run"));
            left_files.push(File::create(&left)?);
            right_files.push(File::create(&right)?);
            self.files.0.push(left.clone());
            self.files.0.push(right.clone());
            left_paths.push(left);
            right_paths.push(right);
        }
        Ok(JoinPartitionSet {
            left_paths,
            right_paths,
            left_files,
            right_files,
        })
    }

    fn record_partition_stats(&self, sizes: &[u64]) {
        for &bytes in sizes {
            if bytes > 0 {
                self.db.record_query_spill(bytes);
            }
        }
    }

    fn process_partition(
        &mut self,
        left_path: &PathBuf,
        right_path: &PathBuf,
        depth: usize,
        right_bytes: u64,
    ) -> Result<()> {
        let build_budget = (self.budget / 2).max(1);
        if right_bytes as usize > build_budget {
            if depth < GRACE_JOIN_MAX_DEPTH {
                let (left, right, right_sizes) =
                    self.repartition(left_path, right_path, depth + 1)?;
                let largest = right_sizes.iter().copied().max().unwrap_or(0);
                if largest < right_bytes {
                    for partition in 0..GRACE_JOIN_PARTITIONS {
                        self.process_partition(
                            &left[partition],
                            &right[partition],
                            depth + 1,
                            right_sizes[partition],
                        )?;
                    }
                    return Ok(());
                }
            }
            return self.process_skewed_partition(left_path, right_path);
        }

        let mut right_reader = SpillFrameReader::open(right_path)?;
        let mut right_rows = Vec::new();
        let mut estimated = 0usize;
        while let Some(record) = right_reader.next_record()? {
            estimated = estimated.saturating_add(record_heap_bytes(&record));
            right_rows.push(record);
        }
        self.db.record_query_buffer(estimated);
        if estimated > build_budget && right_rows.len() > 1 {
            drop(right_rows);
            return self.process_skewed_partition(left_path, right_path);
        }
        self.probe_loaded_partition(left_path, right_rows)
    }

    fn repartition(
        &mut self,
        left_path: &PathBuf,
        right_path: &PathBuf,
        depth: usize,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<u64>)> {
        let mut partitions = self.new_partition_set(&format!("d{depth}"))?;
        let mut left_sizes = vec![0u64; GRACE_JOIN_PARTITIONS];
        let mut reader = SpillFrameReader::open(left_path)?;
        while let Some(row) = reader.next_exec_row()? {
            let key = join_key(&col_value(&row, self.existing.0, &self.existing.1));
            let partition = grace_partition(key.as_deref(), depth as u64);
            left_sizes[partition] +=
                write_spill_exec_row(&mut partitions.left_files[partition], &row)?;
        }
        let mut right_sizes = vec![0u64; GRACE_JOIN_PARTITIONS];
        let mut reader = SpillFrameReader::open(right_path)?;
        while let Some(record) = reader.next_record()? {
            let key = record.get(self.new_col).and_then(join_key);
            let partition = grace_partition(key.as_deref(), depth as u64);
            right_sizes[partition] +=
                write_spill_record(&mut partitions.right_files[partition], &record)?;
        }
        drop(partitions.left_files);
        drop(partitions.right_files);
        self.record_partition_stats(&left_sizes);
        self.record_partition_stats(&right_sizes);
        Ok((partitions.left_paths, partitions.right_paths, right_sizes))
    }

    fn probe_loaded_partition(
        &mut self,
        left_path: &PathBuf,
        right_rows: Vec<Record>,
    ) -> Result<()> {
        let mut table_map: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (index, record) in right_rows.iter().enumerate() {
            if let Some(key) = record.get(self.new_col).and_then(join_key) {
                table_map.entry(key).or_default().push(index);
            }
        }
        let mut matched_right = vec![false; right_rows.len()];
        let mut left_reader = SpillFrameReader::open(left_path)?;
        while let Some(row) = left_reader.next_exec_row()? {
            let key = join_key(&col_value(&row, self.existing.0, &self.existing.1));
            match key.as_ref().and_then(|key| table_map.get(key)) {
                Some(indices) if !indices.is_empty() => {
                    for &index in indices {
                        matched_right[index] = true;
                        self.out.push(widen_join_row(
                            &row,
                            self.new_ti,
                            Some(right_rows[index].clone()),
                        ));
                    }
                }
                _ if self.kind == JoinKind::Left => {
                    self.out.push(widen_join_row(&row, self.new_ti, None));
                }
                _ => {}
            }
        }
        if self.kind == JoinKind::Right {
            for (index, record) in right_rows.into_iter().enumerate() {
                if !matched_right[index] {
                    self.out.push(unmatched_right_row(self.new_ti, record));
                }
            }
        }
        Ok(())
    }

    /// Degenerate-key fallback for a partition that cannot be made smaller by
    /// hashing. The build side is read in bounded chunks and the probe side is
    /// rescanned; outer-match flags live in a temporary file, not RAM.
    fn process_skewed_partition(
        &mut self,
        left_path: &PathBuf,
        right_path: &PathBuf,
    ) -> Result<()> {
        let matched_path = self
            .spill_dir
            .join(format!("hash-{}-matched.run", Ulid::new()));
        self.files.0.push(matched_path.clone());
        let matched_left = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&matched_path)?;
        let left_count = count_spill_rows(left_path)?;
        if self.kind == JoinKind::Left {
            matched_left.set_len(left_count as u64)?;
        }

        let chunk_budget = (self.budget / 2).max(1);
        let mut right_reader = SpillFrameReader::open(right_path)?;
        let mut saw_right = false;
        loop {
            let mut chunk = Vec::new();
            let mut bytes = 0usize;
            while let Some(record) = right_reader.next_record()? {
                saw_right = true;
                bytes = bytes.saturating_add(record_heap_bytes(&record));
                chunk.push(record);
                if bytes >= chunk_budget {
                    break;
                }
            }
            if chunk.is_empty() {
                break;
            }
            self.db.record_query_buffer(bytes);
            let mut table_map: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
            for (index, record) in chunk.iter().enumerate() {
                if let Some(key) = record.get(self.new_col).and_then(join_key) {
                    table_map.entry(key).or_default().push(index);
                }
            }
            let mut matched_right = vec![false; chunk.len()];
            let mut left_reader = SpillFrameReader::open(left_path)?;
            let mut ordinal = 0u64;
            while let Some(row) = left_reader.next_exec_row()? {
                let key = join_key(&col_value(&row, self.existing.0, &self.existing.1));
                if let Some(indices) = key.as_ref().and_then(|key| table_map.get(key)) {
                    if self.kind == JoinKind::Left {
                        matched_left.write_all_at(&[1], ordinal)?;
                    }
                    for &index in indices {
                        matched_right[index] = true;
                        self.out.push(widen_join_row(
                            &row,
                            self.new_ti,
                            Some(chunk[index].clone()),
                        ));
                    }
                }
                ordinal += 1;
            }
            if self.kind == JoinKind::Right {
                for (index, record) in chunk.into_iter().enumerate() {
                    if !matched_right[index] {
                        self.out.push(unmatched_right_row(self.new_ti, record));
                    }
                }
            }
        }

        if self.kind == JoinKind::Left {
            let mut left_reader = SpillFrameReader::open(left_path)?;
            let mut ordinal = 0u64;
            let mut matched = [0u8; 1];
            while let Some(row) = left_reader.next_exec_row()? {
                matched_left.read_exact_at(&mut matched, ordinal)?;
                if matched[0] == 0 {
                    self.out.push(widen_join_row(&row, self.new_ti, None));
                }
                ordinal += 1;
            }
        } else if self.kind == JoinKind::Right && !saw_right {
            // No right rows means there is nothing for a RIGHT JOIN to retain.
        }
        Ok(())
    }
}

fn grace_partition(key: Option<&[u8]>, seed: u64) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for &byte in key.unwrap_or(&[0xff]) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    (hash as usize) & (GRACE_JOIN_PARTITIONS - 1)
}

fn count_spill_rows(path: &PathBuf) -> Result<usize> {
    let mut reader = SpillFrameReader::open(path)?;
    let mut count = 0usize;
    while reader.next_frame()?.is_some() {
        count += 1;
    }
    Ok(count)
}

fn widen_join_row(row: &ExecRow, new_ti: usize, added: Option<Record>) -> ExecRow {
    let mut widened = row.clone();
    widened.resize(new_ti, None);
    widened.push(added);
    widened
}

fn unmatched_right_row(new_ti: usize, record: Record) -> ExecRow {
    let mut row = vec![None; new_ti];
    row.push(Some(record));
    row
}

#[allow(clippy::too_many_arguments)]
fn exec_join(
    db: &Db,
    left_rows: Vec<ExecRow>,
    new_table: &TableCtx,
    new_ti: usize,
    existing: (usize, String),
    new_col: &str,
    kind: JoinKind,
    pushdown: &[RExpr],
) -> Result<Vec<ExecRow>> {
    let mut out: Vec<ExecRow> = Vec::new();

    let use_index_loop = join_uses_index_loop(kind, new_table, new_col);

    if use_index_loop {
        for row in &left_rows {
            let key = col_value(row, existing.0, &existing.1);
            let matches: Vec<Record> = if key.is_null() {
                Vec::new()
            } else if new_col == ID_COLUMN && new_table.schema.has_implicit_id() {
                match &key {
                    Value::Text(id) => db
                        .get_unbudgeted(&new_table.schema.name, id)?
                        .into_iter()
                        .collect(),
                    _ => Vec::new(),
                }
            } else {
                let ty = new_table.schema.column(new_col).expect("resolved").ty;
                match coerce_for_lookup(&key, ty) {
                    Some(k) => db
                        .find_eq_unbudgeted(&new_table.schema.name, new_col, &k)?
                        .into_iter()
                        .map(|(_, r)| r)
                        .collect(),
                    None => Vec::new(),
                }
            };
            let mut emitted = false;
            for m in matches {
                let candidate = single_row(new_ti, m);
                if eval_all(&candidate, pushdown)? {
                    out.push(widen_join_row(
                        row,
                        new_ti,
                        candidate.into_iter().nth(new_ti).flatten(),
                    ));
                    emitted = true;
                }
            }
            if !emitted && kind == JoinKind::Left {
                out.push(widen_join_row(row, new_ti, None));
            }
        }
        return Ok(out);
    }

    GraceHashJoin::new(db, new_ti, existing, new_col, kind).execute(left_rows, new_table, pushdown)
}

// --- INSERT / UPDATE / DELETE ---------------------------------------------------------

fn exec_insert(
    db: &Db,
    table: &str,
    columns: &[String],
    rows: &[Vec<Literal>],
    returning: &[String],
    statement_timestamp: i64,
    ignore_unique: bool,
) -> Result<QueryOutput> {
    if ignore_unique {
        return exec_insert_ignore_unique(db, table, columns, rows, returning, statement_timestamp);
    }
    let mut txn = db.begin();
    let output = exec_insert_txn(
        &mut txn,
        table,
        columns,
        rows,
        returning,
        statement_timestamp,
    )?;
    txn.commit()?;
    Ok(output)
}

fn exec_insert_ignore_unique(
    db: &Db,
    table: &str,
    columns: &[String],
    rows: &[Vec<Literal>],
    returning: &[String],
    statement_timestamp: i64,
) -> Result<QueryOutput> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let returning_columns = if returning.len() == 1 && returning[0] == "*" {
        implicit_id_and_declared_columns(&schema)
    } else {
        returning.to_vec()
    };
    let identity_column = schema
        .columns
        .iter()
        .find(|column| column.identity)
        .map(|column| column.name.clone());
    let mut ids = Vec::new();
    let mut identity_values = Vec::new();
    let mut returned_rows = Vec::new();
    for row in rows {
        match exec_insert(
            db,
            table,
            columns,
            std::slice::from_ref(row),
            returning,
            statement_timestamp,
            false,
        ) {
            Ok(QueryOutput::Inserted { ids: inserted }) => ids.extend(inserted),
            Ok(QueryOutput::InsertedIdentity {
                ids: inserted,
                values,
                ..
            }) => {
                ids.extend(inserted);
                identity_values.extend(values);
            }
            Ok(QueryOutput::Rows { rows, .. }) => returned_rows.extend(rows),
            Ok(other) => unreachable!("INSERT returned {other:?}"),
            Err(Error::UniqueViolation { .. } | Error::DuplicateId { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    if !returning_columns.is_empty() {
        Ok(QueryOutput::Rows {
            columns: returning_columns,
            rows: returned_rows,
        })
    } else if let Some(column) = identity_column {
        Ok(QueryOutput::InsertedIdentity {
            ids,
            column,
            values: identity_values,
        })
    } else {
        Ok(QueryOutput::Inserted { ids })
    }
}

fn exec_insert_txn(
    txn: &mut Txn,
    table: &str,
    columns: &[String],
    rows: &[Vec<Literal>],
    returning: &[String],
    statement_timestamp: i64,
) -> Result<QueryOutput> {
    let schema = txn.table_schema(table)?;
    let inferred_columns;
    let columns = if columns.is_empty() {
        inferred_columns = schema
            .columns
            .iter()
            .filter(|column| !column.identity)
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        inferred_columns.as_slice()
    } else {
        columns
    };
    for (i, c) in columns.iter().enumerate() {
        if !has_col(&schema, c) {
            return Err(Error::Sql(format!(
                "unknown column '{c}' in table '{table}'"
            )));
        }
        if columns[..i].iter().any(|prev| prev == c) {
            return Err(Error::Sql(format!("column '{c}' listed twice")));
        }
    }
    let returning_columns = if returning.len() == 1 && returning[0] == "*" {
        implicit_id_and_declared_columns(&schema)
    } else {
        returning.to_vec()
    };
    for column in &returning_columns {
        if !has_col(&schema, column) {
            return Err(Error::Sql(format!(
                "unknown RETURNING column '{column}' in table '{table}'"
            )));
        }
    }
    let mut ids = Vec::with_capacity(rows.len());
    let mut returned_rows = Vec::with_capacity(rows.len());
    let identity_column = schema
        .columns
        .iter()
        .find(|column| column.identity)
        .map(|column| column.name.clone());
    let mut identity_values = Vec::with_capacity(rows.len());
    for lits in rows {
        if lits.len() != columns.len() {
            return Err(Error::Sql(format!(
                "row has {} values but table '{table}' has {} declared columns",
                lits.len(),
                columns.len()
            )));
        }
        let mut rec = Record::new();
        for (c, lit) in columns.iter().zip(lits) {
            let v = if c == ID_COLUMN && schema.has_implicit_id() {
                match lit {
                    Literal::Str(s) => Value::Text(s.clone()),
                    Literal::Bound(Value::Text(s)) => Value::Text(s.clone()),
                    _ => return Err(Error::Sql("id must be a text value".into())),
                }
            } else {
                let ty = schema.column(c).expect("checked").ty;
                literal_to_value(lit, ty, c)?
            };
            rec.insert(c.clone(), v);
        }
        for column in &schema.columns {
            if column.default_current_timestamp && !rec.contains_key(&column.name) {
                rec.insert(column.name.clone(), Value::Timestamp(statement_timestamp));
            }
        }
        let id = txn.insert(table, rec)?;
        if !returning_columns.is_empty() || identity_column.is_some() {
            let inserted = txn
                .get(table, &id)?
                .expect("a staged insert is visible to its transaction");
            if let Some(identity_column) = &identity_column {
                let Value::Int64(value) = inserted[identity_column] else {
                    unreachable!("identity normalization always writes an int")
                };
                identity_values.push(value);
            }
            if !returning_columns.is_empty() {
                returned_rows.push(
                    returning_columns
                        .iter()
                        .map(|column| inserted[column].clone())
                        .collect(),
                );
            }
        }
        ids.push(id);
    }
    if returning_columns.is_empty() {
        match identity_column {
            Some(column) => Ok(QueryOutput::InsertedIdentity {
                ids,
                column,
                values: identity_values,
            }),
            None => Ok(QueryOutput::Inserted { ids }),
        }
    } else {
        Ok(QueryOutput::Rows {
            columns: returning_columns,
            rows: returned_rows,
        })
    }
}

fn exec_update(
    db: &Db,
    table: &str,
    sets: &[(String, SetValue)],
    where_clause: Option<&Expr>,
) -> Result<QueryOutput> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    for (col, value) in sets {
        if col == ID_COLUMN && schema.has_implicit_id() {
            return Err(Error::Sql("the primary key cannot be updated".into()));
        }
        let target = schema
            .column(col)
            .ok_or_else(|| Error::Sql(format!("unknown column '{col}' in table '{table}'")))?;
        if target.identity {
            return Err(Error::InvalidArgument(format!(
                "identity column '{}' cannot be updated",
                target.name
            )));
        }
        match value {
            SetValue::Literal(literal) => {
                literal_to_value(literal, target.ty, col)?;
            }
            SetValue::Arithmetic {
                column,
                op: _,
                right,
            } => {
                if column != col {
                    return Err(Error::Sql(format!(
                        "SET arithmetic must update a column from itself: use {col} = {col} <op> value"
                    )));
                }
                if !matches!(target.ty, ColumnType::Int64 | ColumnType::Float64) {
                    return Err(Error::Sql(format!(
                        "arithmetic requires a numeric column; '{col}' is {}",
                        target.ty
                    )));
                }
                literal_to_value(right, target.ty, col)?;
            }
        }
    }
    let tables = [TableCtx {
        schema,
        label: table.to_owned(),
    }];
    let conjuncts = resolve_where(&tables, where_clause)?;

    let deadline = Instant::now() + WRITE_RETRY_BUDGET;
    let mut attempt = 0u32;
    loop {
        // The snapshot must predate the read: a commit landing between a read
        // and `begin()` sits inside the snapshot, so the write-write check at
        // commit cannot see it and the patch built from the stale row would
        // silently overwrite it. Reading after `begin()` can only surface rows
        // NEWER than the snapshot, which the version check turns into a
        // Conflict and a retry — never into a lost update.
        let mut txn = db.begin();
        let matching = fetch_table(db, &tables[0], 0, &conjuncts)?;
        let mut count = 0u64;
        for rec in &matching {
            let id = physical_row_id(rec)?;
            let mut patch = Record::new();
            for (column, set) in sets {
                let target = tables[0].schema.column(column).expect("validated above");
                let value = match set {
                    SetValue::Literal(literal) => literal_to_value(literal, target.ty, column)?,
                    SetValue::Arithmetic { op, right, .. } => {
                        let left = rec.get(column).ok_or_else(|| {
                            Error::Corrupt(format!("record is missing declared column '{column}'"))
                        })?;
                        let right = literal_to_value(right, target.ty, column)?;
                        arithmetic_value(left, &right, *op, column)?
                    }
                };
                patch.insert(column.clone(), value);
            }
            match txn.update(table, id, patch) {
                Ok(()) => count += 1,
                Err(Error::RecordNotFound { .. }) => {} // deleted concurrently
                Err(e) => return Err(e),
            }
        }
        match txn.commit() {
            Ok(_) => return Ok(QueryOutput::Affected(count)),
            Err(Error::Conflict(m)) => {
                if Instant::now() >= deadline {
                    return Err(Error::Conflict(m));
                }
                backoff_before_retry(attempt);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn arithmetic_value(left: &Value, right: &Value, op: ArithmeticOp, column: &str) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => {
            let value = match op {
                ArithmeticOp::Add => left.checked_add(*right),
                ArithmeticOp::Subtract => left.checked_sub(*right),
                ArithmeticOp::Multiply => left.checked_mul(*right),
                ArithmeticOp::Divide if *right == 0 => {
                    return Err(Error::Sql(format!(
                        "division by zero while updating '{column}'"
                    )))
                }
                ArithmeticOp::Divide => left.checked_div(*right),
            }
            .ok_or_else(|| Error::Sql(format!("int64 overflow while updating '{column}'")))?;
            Ok(Value::Int64(value))
        }
        (Value::Float64(left), Value::Float64(right)) => {
            if matches!(op, ArithmeticOp::Divide) && *right == 0.0 {
                return Err(Error::Sql(format!(
                    "division by zero while updating '{column}'"
                )));
            }
            let value = match op {
                ArithmeticOp::Add => left + right,
                ArithmeticOp::Subtract => left - right,
                ArithmeticOp::Multiply => left * right,
                ArithmeticOp::Divide => left / right,
            };
            if !value.is_finite() {
                return Err(Error::Sql(format!(
                    "float64 overflow while updating '{column}'"
                )));
            }
            Ok(Value::Float64(value))
        }
        _ => Err(Error::Sql(format!(
            "arithmetic requires matching numeric values for '{column}'"
        ))),
    }
}

fn validate_txn_sets(schema: &TableSchema, table: &str, sets: &[(String, SetValue)]) -> Result<()> {
    for (column, set) in sets {
        if column == ID_COLUMN && schema.has_implicit_id() {
            return Err(Error::Sql("the primary key cannot be updated".into()));
        }
        let target = schema
            .column(column)
            .ok_or_else(|| Error::Sql(format!("unknown column '{column}' in table '{table}'")))?;
        if target.identity {
            return Err(Error::InvalidArgument(format!(
                "identity column '{}' cannot be updated",
                target.name
            )));
        }
        match set {
            SetValue::Literal(literal) => {
                literal_to_value(literal, target.ty, column)?;
            }
            SetValue::Arithmetic {
                column: source,
                right,
                ..
            } => {
                if source != column {
                    return Err(Error::Sql(format!(
                        "SET arithmetic must update a column from itself: use {column} = {column} <op> value"
                    )));
                }
                if !matches!(target.ty, ColumnType::Int64 | ColumnType::Float64) {
                    return Err(Error::Sql(format!(
                        "arithmetic requires a numeric column; '{column}' is {}",
                        target.ty
                    )));
                }
                literal_to_value(right, target.ty, column)?;
            }
        }
    }
    Ok(())
}

fn exec_update_txn(
    txn: &mut Txn,
    table: &str,
    sets: &[(String, SetValue)],
    where_clause: Option<&Expr>,
) -> Result<QueryOutput> {
    let schema = txn.table_schema(table)?;
    validate_txn_sets(&schema, table, sets)?;
    let tables = [TableCtx {
        schema,
        label: table.to_owned(),
    }];
    let predicates = resolve_where(&tables, where_clause)?;
    let matching: Vec<Record> = txn
        .scan(table)?
        .into_iter()
        .map(|(_, record)| record)
        .filter_map(|record| {
            let row = vec![Some(record.clone())];
            match eval_all(&row, &predicates) {
                Ok(true) => Some(Ok(record)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<_>>()?;
    let mut affected = 0u64;
    for record in matching {
        let Value::Text(id) = &record[ID_COLUMN] else {
            return Err(Error::Corrupt("record has non-text id".into()));
        };
        let mut patch = Record::new();
        for (column, set) in sets {
            let target = tables[0].schema.column(column).expect("validated above");
            let value = match set {
                SetValue::Literal(literal) => literal_to_value(literal, target.ty, column)?,
                SetValue::Arithmetic { op, right, .. } => arithmetic_value(
                    record.get(column).ok_or_else(|| {
                        Error::Corrupt(format!("record is missing declared column '{column}'"))
                    })?,
                    &literal_to_value(right, target.ty, column)?,
                    *op,
                    column,
                )?,
            };
            patch.insert(column.clone(), value);
        }
        txn.update(table, id, patch)?;
        affected += 1;
    }
    Ok(QueryOutput::Affected(affected))
}

fn exec_delete_txn(txn: &mut Txn, table: &str, where_clause: Option<&Expr>) -> Result<QueryOutput> {
    let schema = txn.table_schema(table)?;
    let tables = [TableCtx {
        schema,
        label: table.to_owned(),
    }];
    let predicates = resolve_where(&tables, where_clause)?;
    let ids: Vec<String> = txn
        .scan(table)?
        .into_iter()
        .filter_map(|(id, record)| {
            let row = vec![Some(record)];
            match eval_all(&row, &predicates) {
                Ok(true) => Some(Ok(id)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<_>>()?;
    let mut affected = 0u64;
    for id in ids {
        if txn.delete(table, &id)? {
            affected += 1;
        }
    }
    Ok(QueryOutput::Affected(affected))
}

fn exec_select_txn(txn: &mut Txn, statement: &SelectStmt) -> Result<QueryOutput> {
    if !statement.joins.is_empty()
        || !statement.group_by.is_empty()
        || statement.having.is_some()
        || statement
            .projection
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. }))
    {
        return Err(Error::Sql(
            "transactional SELECT currently supports one table without aggregates".into(),
        ));
    }
    let schema = txn.table_schema(&statement.from.name)?;
    let tables = [TableCtx {
        schema: schema.clone(),
        label: statement
            .from
            .alias
            .clone()
            .unwrap_or_else(|| statement.from.name.clone()),
    }];
    let predicates = resolve_where(&tables, statement.where_clause.as_ref())?;
    let mut records: Vec<Record> = txn
        .scan(&statement.from.name)?
        .into_iter()
        .map(|(_, record)| record)
        .filter_map(|record| {
            let row = vec![Some(record.clone())];
            match eval_all(&row, &predicates) {
                Ok(true) => Some(Ok(record)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<_>>()?;
    let order: Vec<_> = statement
        .order_by
        .iter()
        .map(|key| {
            let (table_index, column) = resolve_col(&tables, &key.column)?;
            debug_assert_eq!(table_index, 0);
            Ok((column, key.desc, key.collation))
        })
        .collect::<Result<_>>()?;
    records.sort_by(|left, right| {
        for (column, desc, collation) in &order {
            let ordering = sort_cmp(
                left.get(column).unwrap_or(&Value::Null),
                right.get(column).unwrap_or(&Value::Null),
                *collation,
            );
            if ordering != Ordering::Equal {
                return if *desc { ordering.reverse() } else { ordering };
            }
        }
        Ordering::Equal
    });

    let mut columns = Vec::new();
    let mut projection = Vec::new();
    for item in &statement.projection {
        match item {
            SelectItem::Star => {
                if schema.has_implicit_id() {
                    columns.push(ID_COLUMN.into());
                    projection.push(ID_COLUMN.to_owned());
                }
                for column in &schema.columns {
                    columns.push(column.name.clone());
                    projection.push(column.name.clone());
                }
            }
            SelectItem::Column { col, alias } => {
                resolve_col(&tables, col)?;
                columns.push(alias.clone().unwrap_or_else(|| col.column.clone()));
                projection.push(col.column.clone());
            }
            SelectItem::Aggregate { .. } => unreachable!("rejected above"),
        }
    }
    let offset = limit_to_usize(statement.offset.as_ref()).unwrap_or(0);
    let limit = limit_to_usize(statement.limit.as_ref()).unwrap_or(usize::MAX);
    let rows = records
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|record| {
            projection
                .iter()
                .map(|column| record.get(column).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    Ok(QueryOutput::Rows { columns, rows })
}

fn exec_delete(db: &Db, table: &str, where_clause: Option<&Expr>) -> Result<QueryOutput> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let tables = [TableCtx {
        schema,
        label: table.to_owned(),
    }];
    let conjuncts = resolve_where(&tables, where_clause)?;

    let deadline = Instant::now() + WRITE_RETRY_BUDGET;
    let mut attempt = 0u32;
    loop {
        // Same ordering invariant as exec_update: the snapshot must predate
        // the read, or a row edited out of the WHERE clause between the read
        // and `begin()` is deleted anyway with no conflict raised.
        let mut txn = db.begin();
        let matching = fetch_table(db, &tables[0], 0, &conjuncts)?;
        let mut count = 0u64;
        for rec in &matching {
            let id = physical_row_id(rec)?;
            if txn.delete(table, id)? {
                count += 1;
            }
        }
        match txn.commit() {
            Ok(_) => return Ok(QueryOutput::Affected(count)),
            Err(Error::Conflict(m)) => {
                if Instant::now() >= deadline {
                    return Err(Error::Conflict(m));
                }
                backoff_before_retry(attempt);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn resolve_where(tables: &[TableCtx], where_clause: Option<&Expr>) -> Result<Vec<RExpr>> {
    let mut conjuncts = Vec::new();
    if let Some(w) = where_clause {
        let resolved = resolve_expr(tables, w)?;
        collect_conjuncts(resolved, &mut conjuncts);
    }
    Ok(conjuncts)
}
