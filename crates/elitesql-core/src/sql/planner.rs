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

/// A single-table access path whose physical secondary-index order satisfies
/// the SQL `ORDER BY`. Materialized rows still pass every predicate; only an
/// exact prefix with no residual filter can skip OFFSET records by membership.
pub(super) struct OrderedSecondaryPlan {
    pub(super) columns: Vec<String>,
    pub(super) tuple_prefix: Vec<u8>,
    /// Every pushed predicate is a distinct equality in the tuple prefix.
    /// Only this case can count OFFSET directly from live index entries.
    pub(super) exact_prefix: bool,
}

/// Return an ordered secondary-index plan only when its byte order is the SQL
/// order requested by the statement. This deliberately starts with the small
/// proven subset: ascending values after an equality prefix, and an optional
/// binary physical-id tie breaker. Other types and collations retain the
/// spill-sort path until their ordering equivalence is demonstrated.
pub(super) fn ordered_secondary_plan(
    table: &TableCtx,
    ti: usize,
    predicates: &[RExpr],
    order_keys: &[((usize, String), SortSpec)],
    has_limit: bool,
) -> Option<OrderedSecondaryPlan> {
    if !has_limit || order_keys.is_empty() || order_keys.iter().any(|(_, spec)| spec.desc) {
        return None;
    }

    let mut equalities = std::collections::BTreeMap::<String, Value>::new();
    for predicate in predicates {
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
        if value.is_null() {
            return None;
        }
        let value = coerce_for_lookup(value, table.schema.column(column)?.ty)?;
        match equalities.get(column) {
            Some(existing) if existing != &value => return None,
            Some(_) => {}
            None => {
                equalities.insert(column.clone(), value);
            }
        }
    }

    for def in &table.schema.indexes {
        let columns = def.columns();
        let prefix_len = columns
            .iter()
            .take_while(|column| equalities.contains_key(*column))
            .count();
        // An ordered walk is useful only after an actual equality prefix and
        // at least one indexed ordering component remains.
        if prefix_len == 0 || prefix_len == columns.len() {
            continue;
        }

        let expected_count = columns.len() - prefix_len;
        if !(order_keys.len() == expected_count || order_keys.len() == expected_count + 1) {
            continue;
        }
        let mut compatible = true;
        for (position, ((key_ti, key_column), spec)) in order_keys.iter().enumerate() {
            if *key_ti != ti {
                compatible = false;
                break;
            }
            if position < expected_count {
                let column = &columns[prefix_len + position];
                if key_column != column
                    || !secondary_order_matches_sql(&table.schema, column, *spec)
                {
                    compatible = false;
                    break;
                }
                continue;
            }
            // Every secondary pair ends in its physical identity. Exposing
            // that as the final binary id key makes ties fully deterministic.
            if key_column != ID_COLUMN
                || !table.schema.has_implicit_id()
                || spec.collation != Collation::Binary
            {
                compatible = false;
            }
        }
        if !compatible {
            continue;
        }

        let mut tuple_prefix = Vec::new();
        for column in &columns[..prefix_len] {
            crate::value::encode_index_value(
                &mut tuple_prefix,
                equalities.get(column).expect("equality prefix was counted"),
            );
        }
        return Some(OrderedSecondaryPlan {
            columns: columns.to_vec(),
            tuple_prefix,
            exact_prefix: predicates.len() == prefix_len && equalities.len() == prefix_len,
        });
    }
    None
}

fn secondary_order_matches_sql(schema: &TableSchema, column: &str, spec: SortSpec) -> bool {
    match schema.column(column).map(|column| column.ty) {
        Some(
            ColumnType::Bool
            | ColumnType::Int64
            | ColumnType::Timestamp
            | ColumnType::Date
            | ColumnType::Time,
        ) => true,
        Some(ColumnType::Text) => spec.collation == Collation::Binary,
        _ => false,
    }
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
        if column == ID_COLUMN {
            // Either form of `id` addresses the primary directory directly.
            // A declared identity is the row's physical key, so reaching it
            // no longer goes through the unique secondary index that used to
            // translate one into the other.
            if table.schema.has_implicit_id() {
                match value {
                    Value::Text(id) => best = Some((0, TableDriver::Id(id.clone()), position)),
                    _ => return (TableDriver::Empty, Some(position)),
                }
                continue;
            }
            if table.schema.keyed_by_identity() {
                match value {
                    Value::Int64(identity) if *identity >= 1 => {
                        best = Some((
                            0,
                            TableDriver::Id(crate::db::identity_key(*identity)),
                            position,
                        ))
                    }
                    Value::Int64(_) => return (TableDriver::Empty, Some(position)),
                    _ => {}
                }
                if best.is_some() {
                    continue;
                }
            }
        }
        let ty = table.schema.column(column).expect("resolved column").ty;
        if let Some(value) = coerce_for_lookup(value, ty) {
            let priority = table
                .schema
                .indexes
                .iter()
                .find(|index| index.columns().len() == 1 && index.column == *column)
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

/// True when the access path can return at most one row: a physical-id
/// lookup, or an equality on a column with a unique index. Callers then fetch
/// a single row and stop, instead of asking for a whole batch and issuing a
/// second lookup to discover the table has no more matches.
pub(super) fn driver_yields_at_most_one_row(table: &TableCtx, driver: &TableDriver) -> bool {
    match driver {
        TableDriver::Empty | TableDriver::Id(_) => true,
        TableDriver::Equality(column, _) => table
            .schema
            .indexes
            .iter()
            .any(|index| index.unique && index.columns().len() == 1 && index.column == *column),
        TableDriver::Scan => false,
    }
}

pub(super) fn has_secondary_index(table: &TableCtx, column: &str) -> bool {
    table
        .schema
        .indexes
        .iter()
        .any(|index| index.columns().len() == 1 && index.column == column)
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
    let ordered = if !is_aggregate && tables.len() == 1 {
        let order_keys = stmt
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
            .collect::<Result<Vec<_>>>()?;
        ordered_secondary_plan(
            &tables[0],
            0,
            &pushdown[0],
            &order_keys,
            stmt.limit.is_some(),
        )
    } else {
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
    if !stmt.order_by.is_empty() && ordered.is_none() {
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
    if let Some(ordered) = ordered {
        plan.line(
            depth,
            format!(
                "INDEX ORDERED {} ({})",
                tables[0].label,
                ordered.columns.join(", ")
            ),
        );
    } else {
        explain_join_tree(
            &mut plan,
            depth,
            stmt,
            &tables,
            &pushdown,
            stmt.joins.len(),
            streamed,
        )?;
    }
    Ok(plan.finish())
}
