//! Planner and executor for the SQL V1 subset.
//!
//! Heuristic planning, no cost model: point lookup on `id`, secondary index
//! on indexed equality, full scan otherwise. Single-table WHERE conjuncts are
//! pushed below joins. Joins run as index nested-loop when the probe side is
//! small and the join column is indexed (or is `id`), hash join otherwise.
//! RIGHT JOIN is executed by preserving the new table's side, i.e. a LEFT
//! JOIN with roles swapped.
//!
//! Consistency: SELECT reads the latest committed state (read-committed);
//! snapshot-consistent reads are available through the Rust API (`scan_at`).
//! UPDATE/DELETE run their write set through a transaction and retry on
//! optimistic conflict.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::db::{Db, Record};
use crate::error::{Error, Result};
use crate::schema::TableSchema;
use crate::value::{encode_value, ColumnType, Value};

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
    /// UPDATE/DELETE: number of records affected.
    Affected(u64),
    /// DDL statements.
    None,
}

const INDEX_JOIN_MAX_PROBE: usize = 1024;
const WRITE_RETRIES: usize = 3;

pub(crate) fn execute(db: &Db, sql: &str) -> Result<QueryOutput> {
    match parser::parse(sql)? {
        Statement::CreateTable { name, columns } => {
            let mut cols = Vec::with_capacity(columns.len());
            for c in columns {
                let mut col = match c.dim {
                    Some(dim) => crate::schema::Column::vector(c.name, dim),
                    None => crate::schema::Column::new(c.name, c.ty),
                };
                if c.not_null {
                    col = col.not_null();
                }
                cols.push(col);
            }
            db.create_table(TableSchema::new(name, cols))?;
            Ok(QueryOutput::None)
        }
        Statement::CreateIndex { table, column, unique } => {
            db.create_index(&table, &column, unique)?;
            Ok(QueryOutput::None)
        }
        Statement::Insert { table, columns, rows } => exec_insert(db, &table, &columns, &rows),
        Statement::Select(stmt) => exec_select(db, &stmt),
        Statement::Update { table, sets, where_clause } => {
            exec_update(db, &table, &sets, where_clause.as_ref())
        }
        Statement::Delete { table, where_clause } => {
            exec_delete(db, &table, where_clause.as_ref())
        }
    }
}

// --- literals ------------------------------------------------------------------

/// Schema-aware coercion for INSERT/UPDATE SET values.
fn literal_to_value(lit: &Literal, ty: ColumnType, col: &str) -> Result<Value> {
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
        _ => {
            return Err(Error::Sql(format!(
                "literal {lit:?} is not valid for column '{col}' of type {ty}"
            )))
        }
    };
    Ok(out)
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
fn sort_cmp(a: &Value, b: &Value) -> Ordering {
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
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
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
    Cmp { left: RVal, op: CmpOp, right: RVal },
    IsNull { col: (usize, String), negated: bool },
    InList { col: (usize, String), list: Vec<Value>, negated: bool },
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
    name == "id" || schema.column(name).is_some()
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

/// Simplified two-valued logic: comparisons involving NULL are false.
fn eval(row: &ExecRow, e: &RExpr) -> Result<bool> {
    eval_ctx(row, e, None)
}

fn eval_ctx(row: &ExecRow, e: &RExpr, having: Option<HavingCtx>) -> Result<bool> {
    Ok(match e {
        RExpr::Cmp { left, op, right } => {
            let a = rval_value(row, left, having);
            let b = rval_value(row, right, having);
            if a.is_null() || b.is_null() {
                return Ok(false);
            }
            match op {
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
            }
        }
        RExpr::IsNull { col, negated } => {
            let v = ctx_col_value(row, col, having);
            v.is_null() != *negated
        }
        RExpr::InList { col, list, negated } => {
            let v = ctx_col_value(row, col, having);
            if v.is_null() {
                return Ok(false);
            }
            let mut hit = false;
            for item in list {
                if item.is_null() {
                    continue;
                }
                if eq_vals(&v, item).ok_or_else(|| not_comparable(&v, item))? {
                    hit = true;
                    break;
                }
            }
            hit != *negated
        }
        RExpr::And(a, b) => eval_ctx(row, a, having)? && eval_ctx(row, b, having)?,
        RExpr::Or(a, b) => eval_ctx(row, a, having)? || eval_ctx(row, b, having)?,
        RExpr::Not(inner) => !eval_ctx(row, inner, having)?,
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

/// Coerce a plain value to a column's type for index lookups.
/// Returns None when the coercion is lossy or impossible (fall back to scan).
fn coerce_for_index(v: &Value, ty: ColumnType) -> Option<Value> {
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

/// Fetch a table's rows applying its pushed-down conjuncts, choosing point
/// lookup on id, indexed equality, or full scan.
fn fetch_table(db: &Db, ctx: &TableCtx, ti: usize, conjuncts: &[RExpr]) -> Result<Vec<Record>> {
    let mut candidates: Option<Vec<Record>> = None;
    for c in conjuncts {
        let RExpr::Cmp { left, op: CmpOp::Eq, right } = c else { continue };
        let (col, val) = match (left, right) {
            (RVal::Col(i, name), RVal::Val(v)) if *i == ti => (name, v),
            (RVal::Val(v), RVal::Col(i, name)) if *i == ti => (name, v),
            _ => continue,
        };
        if val.is_null() {
            candidates = Some(Vec::new());
            break;
        }
        if col == "id" {
            let Value::Text(id) = val else {
                candidates = Some(Vec::new());
                break;
            };
            candidates = Some(db.get(&ctx.schema.name, id)?.into_iter().collect());
            break;
        }
        let indexed = ctx.schema.indexes.iter().any(|d| &d.column == col);
        if indexed {
            let ty = ctx.schema.column(col).expect("resolved").ty;
            if let Some(key) = coerce_for_index(val, ty) {
                let hits = db.find_eq(&ctx.schema.name, col, &key)?;
                candidates = Some(hits.into_iter().map(|(_, r)| r).collect());
                break;
            }
        }
    }
    let rows = match candidates {
        Some(rows) => rows,
        None => db.scan(&ctx.schema.name)?.into_iter().map(|(_, r)| r).collect(),
    };
    // Apply every conjunct (re-checking the driving one is harmless).
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let row: ExecRow = single_row(ti, r);
        if eval_all(&row, conjuncts)? {
            out.push(take_single(row, ti));
        }
    }
    Ok(out)
}

fn single_row(ti: usize, rec: Record) -> ExecRow {
    let mut row: ExecRow = vec![None; ti + 1];
    row[ti] = Some(rec);
    row
}

fn take_single(mut row: ExecRow, ti: usize) -> Record {
    row[ti].take().expect("present")
}

fn eval_all(row: &ExecRow, conjuncts: &[RExpr]) -> Result<bool> {
    for c in conjuncts {
        if !eval(row, c)? {
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

// --- SELECT -----------------------------------------------------------------------

fn exec_select(db: &Db, stmt: &SelectStmt) -> Result<QueryOutput> {
    // Resolve FROM + JOIN tables.
    let mut tables: Vec<TableCtx> = Vec::new();
    let mut load = |tref: &TableRef| -> Result<()> {
        let schema = db
            .table_schema(&tref.name)
            .ok_or_else(|| Error::TableNotFound(tref.name.clone()))?;
        let label = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
        if tables.iter().any(|t: &TableCtx| t.label.eq_ignore_ascii_case(&label)) {
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
    let is_aggregate = stmt
        .projection
        .iter()
        .any(|i| matches!(i, SelectItem::Aggregate { .. }))
        || !stmt.group_by.is_empty()
        || stmt.having.is_some();
    if is_aggregate {
        return exec_aggregate(&tables, rows, stmt);
    }

    // ORDER BY.
    if !stmt.order_by.is_empty() {
        let keys: Vec<((usize, String), bool)> = stmt
            .order_by
            .iter()
            .map(|(c, desc)| Ok((resolve_col(&tables, c)?, *desc)))
            .collect::<Result<_>>()?;
        rows.sort_by(|a, b| {
            for ((ti, col), desc) in &keys {
                let va = col_value(a, *ti, col);
                let vb = col_value(b, *ti, col);
                let ord = sort_cmp(&va, &vb);
                if ord != Ordering::Equal {
                    return if *desc { ord.reverse() } else { ord };
                }
            }
            Ordering::Equal
        });
    }

    // OFFSET / LIMIT.
    let offset = stmt.offset.unwrap_or(0) as usize;
    if offset > 0 {
        rows = rows.into_iter().skip(offset).collect();
    }
    if let Some(limit) = stmt.limit {
        rows.truncate(limit as usize);
    }

    // Projection.
    let single = tables.len() == 1;
    let mut columns: Vec<String> = Vec::new();
    let mut extract: Vec<(usize, String)> = Vec::new();
    for item in &stmt.projection {
        match item {
            SelectItem::Star => {
                for (ti, t) in tables.iter().enumerate() {
                    let prefix = if single { String::new() } else { format!("{}.", t.label) };
                    columns.push(format!("{prefix}id"));
                    extract.push((ti, "id".into()));
                    for c in &t.schema.columns {
                        columns.push(format!("{prefix}{}", c.name));
                        extract.push((ti, c.name.clone()));
                    }
                }
            }
            SelectItem::Column { col, alias } => {
                let (ti, name) = resolve_col(&tables, col)?;
                let header = alias.clone().unwrap_or_else(|| name.clone());
                columns.push(header);
                extract.push((ti, name));
            }
            SelectItem::Aggregate { .. } => {
                unreachable!("aggregate queries are handled by exec_aggregate")
            }
        }
    }
    let out_rows: Vec<Vec<Value>> = rows
        .iter()
        .map(|row| {
            extract
                .iter()
                .map(|(ti, col)| col_value(row, *ti, col))
                .collect()
        })
        .collect();
    Ok(QueryOutput::Rows { columns, rows: out_rows })
}

// --- aggregation --------------------------------------------------------------

#[derive(PartialEq)]
struct AggSpec {
    func: AggFunc,
    arg: Option<(usize, String)>,
}

enum AggState {
    Count(u64),
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
    fn new(func: AggFunc) -> AggState {
        match func {
            AggFunc::Count => AggState::Count(0),
            AggFunc::Sum => AggState::Sum {
                ints: 0,
                floats: 0.0,
                saw_float: false,
                saw_any: false,
            },
            AggFunc::Avg => AggState::Avg { sum: 0.0, n: 0 },
            AggFunc::Min => AggState::MinMax { best: None, is_min: true },
            AggFunc::Max => AggState::MinMax { best: None, is_min: false },
        }
    }

    /// SQL NULL semantics: COUNT(col)/SUM/AVG/MIN/MAX ignore NULLs;
    /// COUNT(*) counts rows.
    fn update(&mut self, spec: &AggSpec, row: &ExecRow) -> Result<()> {
        match self {
            AggState::Count(n) => match &spec.arg {
                None => *n += 1,
                Some((ti, col)) => {
                    if !col_value(row, *ti, col).is_null() {
                        *n += 1;
                    }
                }
            },
            AggState::Sum { ints, floats, saw_float, saw_any } => {
                let (ti, col) = spec.arg.as_ref().expect("SUM has a column");
                match col_value(row, *ti, col) {
                    Value::Null => {}
                    Value::Int64(x) => {
                        *ints += x as i128;
                        *saw_any = true;
                    }
                    Value::Float64(f) => {
                        *floats += f;
                        *saw_float = true;
                        *saw_any = true;
                    }
                    other => {
                        return Err(Error::Sql(format!("SUM over non-numeric value {other:?}")))
                    }
                }
            }
            AggState::Avg { sum, n } => {
                let (ti, col) = spec.arg.as_ref().expect("AVG has a column");
                match col_value(row, *ti, col) {
                    Value::Null => {}
                    Value::Int64(x) => {
                        *sum += x as f64;
                        *n += 1;
                    }
                    Value::Float64(f) => {
                        *sum += f;
                        *n += 1;
                    }
                    other => {
                        return Err(Error::Sql(format!("AVG over non-numeric value {other:?}")))
                    }
                }
            }
            AggState::MinMax { best, is_min } => {
                let (ti, col) = spec.arg.as_ref().expect("MIN/MAX have a column");
                let v = col_value(row, *ti, col);
                if v.is_null() {
                    return Ok(());
                }
                match best {
                    None => *best = Some(v),
                    Some(b) => {
                        let ord = cmp_vals(b, &v).ok_or_else(|| not_comparable(b, &v))?;
                        let replace = if *is_min {
                            ord == Ordering::Greater
                        } else {
                            ord == Ordering::Less
                        };
                        if replace {
                            *best = Some(v);
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
            AggState::Sum { ints, floats, saw_float, saw_any } => {
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
    if col == "id" {
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
        Operand::Agg { func, arg } => {
            let arg_r = match arg {
                Some(c) => Some(resolve_col(tables, c)?),
                None => None,
            };
            validate_agg(tables, *func, &arg_r)?;
            RVal::Agg(agg_index(aggs, AggSpec { func: *func, arg: arg_r }))
        }
    })
}

enum OutCol {
    Group(usize),
    Agg(usize),
}

/// Hash aggregation with first-seen group order, HAVING over group columns
/// and aggregates, ORDER BY over the output headers, then OFFSET/LIMIT.
fn exec_aggregate(tables: &[TableCtx], rows: Vec<ExecRow>, stmt: &SelectStmt) -> Result<QueryOutput> {
    let group_cols: Vec<(usize, String)> = stmt
        .group_by
        .iter()
        .map(|c| resolve_col(tables, c))
        .collect::<Result<_>>()?;
    let group_set: HashSet<(usize, String)> = group_cols.iter().cloned().collect();

    let mut specs: Vec<AggSpec> = Vec::new();
    let mut headers: Vec<String> = Vec::new();
    let mut out_cols: Vec<OutCol> = Vec::new();
    for item in &stmt.projection {
        match item {
            SelectItem::Star => {
                return Err(Error::Sql(
                    "SELECT * cannot be combined with aggregates/GROUP BY; list columns explicitly"
                        .into(),
                ))
            }
            SelectItem::Column { col, alias } => {
                let rc = resolve_col(tables, col)?;
                let Some(gi) = group_cols.iter().position(|g| *g == rc) else {
                    return Err(Error::Sql(format!(
                        "column '{}' must appear in GROUP BY",
                        col.column
                    )));
                };
                headers.push(alias.clone().unwrap_or_else(|| rc.1.clone()));
                out_cols.push(OutCol::Group(gi));
            }
            SelectItem::Aggregate { func, arg, alias } => {
                let arg_r = match arg {
                    Some(c) => Some(resolve_col(tables, c)?),
                    None => None,
                };
                validate_agg(tables, *func, &arg_r)?;
                let default = match &arg_r {
                    Some((_, name)) => format!("{}({name})", func.name()),
                    None => format!("{}(*)", func.name()),
                };
                let idx = agg_index(&mut specs, AggSpec { func: *func, arg: arg_r });
                headers.push(alias.clone().unwrap_or(default));
                out_cols.push(OutCol::Agg(idx));
            }
        }
    }
    let having_r = match &stmt.having {
        Some(h) => Some(resolve_having_expr(tables, h, &group_set, &mut specs)?),
        None => None,
    };

    // Group in first-seen order.
    let new_states = |specs: &[AggSpec]| -> Vec<AggState> {
        specs.iter().map(|s| AggState::new(s.func)).collect()
    };
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
            if !eval_ctx(&empty_row, h, Some((&group_map, &agg_vals)))? {
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
        let keys: Vec<(usize, bool)> = stmt
            .order_by
            .iter()
            .map(|(c, desc)| {
                headers
                    .iter()
                    .position(|h| h.eq_ignore_ascii_case(&c.column))
                    .map(|pos| (pos, *desc))
                    .ok_or_else(|| {
                        Error::Sql(format!(
                            "in aggregate queries ORDER BY must reference an output column or alias; '{}' is neither",
                            c.column
                        ))
                    })
            })
            .collect::<Result<_>>()?;
        out_rows.sort_by(|a, b| {
            for (i, desc) in &keys {
                let ord = sort_cmp(&a[*i], &b[*i]);
                if ord != Ordering::Equal {
                    return if *desc { ord.reverse() } else { ord };
                }
            }
            Ordering::Equal
        });
    }

    let offset = stmt.offset.unwrap_or(0) as usize;
    if offset > 0 {
        out_rows = out_rows.into_iter().skip(offset).collect();
    }
    if let Some(limit) = stmt.limit {
        out_rows.truncate(limit as usize);
    }
    Ok(QueryOutput::Rows { columns: headers, rows: out_rows })
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
    let widen = |row: &ExecRow, added: Option<Record>| -> ExecRow {
        let mut r = row.clone();
        r.resize(new_ti, None);
        r.push(added);
        r
    };

    let indexed = new_col == "id"
        || new_table.schema.indexes.iter().any(|d| d.column == new_col);
    let use_index_loop =
        kind != JoinKind::Right && indexed && left_rows.len() <= INDEX_JOIN_MAX_PROBE;

    if use_index_loop {
        for row in &left_rows {
            let key = col_value(row, existing.0, &existing.1);
            let matches: Vec<Record> = if key.is_null() {
                Vec::new()
            } else if new_col == "id" {
                match &key {
                    Value::Text(id) => db.get(&new_table.schema.name, id)?.into_iter().collect(),
                    _ => Vec::new(),
                }
            } else {
                let ty = new_table.schema.column(new_col).expect("resolved").ty;
                match coerce_for_index(&key, ty) {
                    Some(k) => db
                        .find_eq(&new_table.schema.name, new_col, &k)?
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
                    out.push(widen(row, candidate.into_iter().nth(new_ti).flatten()));
                    emitted = true;
                }
            }
            if !emitted && kind == JoinKind::Left {
                out.push(widen(row, None));
            }
        }
        return Ok(out);
    }

    // Hash join: build on the new table, probe with the accumulated rows.
    let new_rows = fetch_table(db, new_table, new_ti, pushdown)?;
    let mut table_map: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, rec) in new_rows.iter().enumerate() {
        if let Some(k) = rec.get(new_col).and_then(join_key) {
            table_map.entry(k).or_default().push(i);
        }
    }
    let mut matched_new: Vec<bool> = vec![false; new_rows.len()];
    for row in &left_rows {
        let key = col_value(row, existing.0, &existing.1);
        let hits = join_key(&key).and_then(|k| table_map.get(&k));
        match hits {
            Some(indices) if !indices.is_empty() => {
                for &i in indices {
                    matched_new[i] = true;
                    out.push(widen(row, Some(new_rows[i].clone())));
                }
            }
            _ => {
                if kind == JoinKind::Left {
                    out.push(widen(row, None));
                }
            }
        }
    }
    if kind == JoinKind::Right {
        for (i, rec) in new_rows.into_iter().enumerate() {
            if !matched_new[i] {
                let mut row: ExecRow = vec![None; new_ti];
                row.push(Some(rec));
                out.push(row);
            }
        }
    }
    Ok(out)
}

// --- INSERT / UPDATE / DELETE ---------------------------------------------------------

fn exec_insert(db: &Db, table: &str, columns: &[String], rows: &[Vec<Literal>]) -> Result<QueryOutput> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    for (i, c) in columns.iter().enumerate() {
        if c != "id" && schema.column(c).is_none() {
            return Err(Error::Sql(format!("unknown column '{c}' in table '{table}'")));
        }
        if columns[..i].iter().any(|prev| prev == c) {
            return Err(Error::Sql(format!("column '{c}' listed twice")));
        }
    }
    let mut txn = db.begin();
    let mut ids = Vec::with_capacity(rows.len());
    for lits in rows {
        let mut rec = Record::new();
        for (c, lit) in columns.iter().zip(lits) {
            let v = if c == "id" {
                match lit {
                    Literal::Str(s) => Value::Text(s.clone()),
                    _ => return Err(Error::Sql("id must be a text literal".into())),
                }
            } else {
                let ty = schema.column(c).expect("checked").ty;
                literal_to_value(lit, ty, c)?
            };
            rec.insert(c.clone(), v);
        }
        ids.push(txn.insert(table, rec)?);
    }
    txn.commit()?;
    Ok(QueryOutput::Inserted { ids })
}

fn exec_update(
    db: &Db,
    table: &str,
    sets: &[(String, Literal)],
    where_clause: Option<&Expr>,
) -> Result<QueryOutput> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let mut patch = Record::new();
    for (col, lit) in sets {
        if col == "id" {
            return Err(Error::Sql("the primary key cannot be updated".into()));
        }
        let ty = schema
            .column(col)
            .ok_or_else(|| Error::Sql(format!("unknown column '{col}' in table '{table}'")))?
            .ty;
        patch.insert(col.clone(), literal_to_value(lit, ty, col)?);
    }
    let tables = [TableCtx { schema, label: table.to_owned() }];
    let conjuncts = resolve_where(&tables, where_clause)?;

    let mut last_err = None;
    for _ in 0..WRITE_RETRIES {
        let matching = fetch_table(db, &tables[0], 0, &conjuncts)?;
        let mut txn = db.begin();
        let mut count = 0u64;
        for rec in &matching {
            let Some(Value::Text(id)) = rec.get("id") else { continue };
            match txn.update(table, id, patch.clone()) {
                Ok(()) => count += 1,
                Err(Error::RecordNotFound { .. }) => {} // deleted concurrently
                Err(e) => return Err(e),
            }
        }
        match txn.commit() {
            Ok(_) => return Ok(QueryOutput::Affected(count)),
            Err(Error::Conflict(m)) => last_err = Some(Error::Conflict(m)),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("retries ran"))
}

fn exec_delete(db: &Db, table: &str, where_clause: Option<&Expr>) -> Result<QueryOutput> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let tables = [TableCtx { schema, label: table.to_owned() }];
    let conjuncts = resolve_where(&tables, where_clause)?;

    let mut last_err = None;
    for _ in 0..WRITE_RETRIES {
        let matching = fetch_table(db, &tables[0], 0, &conjuncts)?;
        let mut txn = db.begin();
        let mut count = 0u64;
        for rec in &matching {
            let Some(Value::Text(id)) = rec.get("id") else { continue };
            if txn.delete(table, id)? {
                count += 1;
            }
        }
        match txn.commit() {
            Ok(_) => return Ok(QueryOutput::Affected(count)),
            Err(Error::Conflict(m)) => last_err = Some(Error::Conflict(m)),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("retries ran"))
}

fn resolve_where(tables: &[TableCtx], where_clause: Option<&Expr>) -> Result<Vec<RExpr>> {
    let mut conjuncts = Vec::new();
    if let Some(w) = where_clause {
        let resolved = resolve_expr(tables, w)?;
        collect_conjuncts(resolved, &mut conjuncts);
    }
    Ok(conjuncts)
}
