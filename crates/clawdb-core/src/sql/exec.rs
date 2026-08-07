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
use std::collections::HashMap;

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
fn cmp_vals(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
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
    })
}

type ExecRow = Vec<Option<Record>>;

fn col_value(row: &ExecRow, ti: usize, col: &str) -> Value {
    row.get(ti)
        .and_then(|r| r.as_ref())
        .and_then(|r| r.get(col).cloned())
        .unwrap_or(Value::Null)
}

fn rval_value(row: &ExecRow, rv: &RVal) -> Value {
    match rv {
        RVal::Col(ti, col) => col_value(row, *ti, col),
        RVal::Val(v) => v.clone(),
    }
}

/// Simplified two-valued logic: comparisons involving NULL are false.
fn eval(row: &ExecRow, e: &RExpr) -> Result<bool> {
    Ok(match e {
        RExpr::Cmp { left, op, right } => {
            let a = rval_value(row, left);
            let b = rval_value(row, right);
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
            let v = col_value(row, col.0, &col.1);
            v.is_null() != *negated
        }
        RExpr::InList { col, list, negated } => {
            let v = col_value(row, col.0, &col.1);
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
        RExpr::And(a, b) => eval(row, a)? && eval(row, b)?,
        RExpr::Or(a, b) => eval(row, a)? || eval(row, b)?,
        RExpr::Not(inner) => !eval(row, inner)?,
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
