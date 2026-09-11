//! SQL planner implementation shared by execution and its tests.
use super::*;

/// How a table's rows are produced. This is the whole access-path decision:
/// every read path (batched SELECT, joins, UPDATE/DELETE) and `EXPLAIN` derive
/// it from `table_driver`, so what EXPLAIN prints is what the executor runs.
pub(super) enum TableDriver {
    /// The predicate cannot match any row, so nothing is read at all.
    Empty,
    Id(String),
    Equality(String, Value),
    Scan,
}

pub(super) fn table_driver(table: &TableCtx, ti: usize, predicates: &[RExpr]) -> TableDriver {
    table_driver_at(table, ti, predicates).0
}

/// `table_driver` plus the index of the predicate that drove the choice.
/// Execution re-checks every predicate anyway, so only `EXPLAIN` needs it — to
/// avoid echoing the driving predicate as a redundant filter line.
pub(super) fn table_driver_at(
    table: &TableCtx,
    ti: usize,
    predicates: &[RExpr],
) -> (TableDriver, Option<usize>) {
    let mut best: Option<(usize, TableDriver, usize)> = None;
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
            match value {
                Value::Text(id) => best = Some((0, TableDriver::Id(id.clone()), position)),
                _ => return (TableDriver::Empty, Some(position)),
            }
            continue;
        }
        let ty = table.schema.column(column).expect("resolved column").ty;
        if let Some(value) = coerce_for_lookup(value, ty) {
            let priority = table
                .schema
                .indexes
                .iter()
                .find(|index| index.column == *column)
                .map_or(3, |index| if index.unique { 1 } else { 2 });
            if best
                .as_ref()
                .is_none_or(|(current, _, _)| priority < *current)
            {
                best = Some((
                    priority,
                    TableDriver::Equality(column.clone(), value),
                    position,
                ));
            }
        }
    }
    best.map(|(_, driver, position)| (driver, Some(position)))
        .unwrap_or((TableDriver::Scan, None))
}

pub(super) fn has_secondary_index(table: &TableCtx, column: &str) -> bool {
    table.schema.indexes.iter().any(|d| d.column == column)
}

/// True when `column` on `table` can be probed directly instead of scanned.
pub(super) fn column_is_probeable(table: &TableCtx, column: &str) -> bool {
    (column == ID_COLUMN && table.schema.has_implicit_id()) || has_secondary_index(table, column)
}

/// The join-strategy decision, shared by the executor and `EXPLAIN`.
///
/// Index nested-loop is preferred at every cardinality: unlike the hash path
/// it does not materialize the complete right table and its hash map. The
/// optimizer may later add a cost-based crossover constrained by memory.
/// RIGHT JOIN always takes the hash path, which is what preserves its side.
pub(super) fn join_uses_index_loop(kind: JoinKind, new_table: &TableCtx, new_col: &str) -> bool {
    kind != JoinKind::Right && column_is_probeable(new_table, new_col)
}

// --- EXPLAIN ------------------------------------------------------------------
//
// EXPLAIN re-derives the plan from the same functions the executor obeys
// (`resolve_query`, `table_driver`, `join_uses_index_loop`, `aggregate_plan`)
// and never runs the query. Planning carries no estimates, so every line is a
// statement about what will happen, not a prediction.

/// Long text values are elided: a plan line must stay readable in a terminal.
const EXPLAIN_TEXT_LIMIT: usize = 32;

pub(super) fn explain_value(value: &Value) -> String {
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

pub(super) fn explain_col(tables: &[TableCtx], col: &(usize, String)) -> String {
    format!("{}.{}", tables[col.0].label, col.1)
}

pub(super) fn explain_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Neq => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

pub(super) fn explain_rval(tables: &[TableCtx], aggs: &[String], value: &RVal) -> String {
    match value {
        RVal::Col(ti, column) => format!("{}.{column}", tables[*ti].label),
        RVal::Val(v) => explain_value(v),
        RVal::Agg(index) => aggs
            .get(*index)
            .cloned()
            .unwrap_or_else(|| format!("agg#{index}")),
    }
}

pub(super) fn explain_expr(tables: &[TableCtx], aggs: &[String], expr: &RExpr) -> String {
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

pub(super) fn explain_select(db: &Db, stmt: &SelectStmt) -> Result<QueryOutput> {
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
