//! EXPLAIN: the plan the executor will actually run.
//!
//! Planning is static and estimate-free, so these assertions pin exact text.
//! If a plan line changes, either the planner changed or EXPLAIN drifted from
//! it — both are worth a failing test.

use elitesql_core::{Db, QueryOutput, Value};
use tempfile::TempDir;

fn seeded() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("explain.esql")).unwrap();
    db.query("CREATE TABLE users (name text, email text, age int64, city text)")
        .unwrap();
    db.query("CREATE TABLE orders (user_id text, total int64, status text)")
        .unwrap();
    db.query("CREATE UNIQUE INDEX ON users (email)").unwrap();
    db.query("CREATE INDEX ON orders (user_id)").unwrap();
    (dir, db)
}

fn plan(db: &Db, sql: &str) -> Vec<String> {
    match db.query(sql).unwrap() {
        QueryOutput::Rows { columns, rows } => {
            assert_eq!(columns, vec!["plan".to_string()]);
            rows.into_iter()
                .map(|row| match row.into_iter().next() {
                    Some(Value::Text(line)) => line,
                    other => panic!("expected a text plan line, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn error(db: &Db, sql: &str) -> String {
    db.query(sql).unwrap_err().to_string()
}

#[test]
fn full_scan_is_reported_as_a_scan() {
    let (_d, db) = seeded();
    assert_eq!(plan(&db, "EXPLAIN SELECT * FROM users"), ["SCAN users"]);
    assert_eq!(
        plan(&db, "EXPLAIN SELECT * FROM users WHERE age >= 30"),
        ["SCAN users", "  filter: users.age >= 30"]
    );
}

#[test]
fn id_equality_is_a_point_lookup() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(&db, "EXPLAIN SELECT * FROM users WHERE id = '01HZZ'"),
        ["POINT LOOKUP users.id = '01HZZ'"]
    );
}

#[test]
fn indexed_equality_uses_the_index_and_unindexed_equality_does_not() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT name FROM users WHERE email = 'ada@example.com'"
        ),
        ["INDEX LOOKUP users.email = 'ada@example.com'"]
    );
    // The distinction EXPLAIN exists for: same shape of predicate, no index,
    // so find_eq walks the primary directory and it costs a full scan.
    assert_eq!(
        plan(&db, "EXPLAIN SELECT name FROM users WHERE city = 'madrid'"),
        ["SCAN users  (equality city = 'madrid', no index)"]
    );
}

#[test]
fn equality_against_null_reads_nothing() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(&db, "EXPLAIN SELECT * FROM users WHERE city = NULL"),
        ["NO ACCESS users  (equality on NULL matches no row)"]
    );
}

#[test]
fn sort_and_limit_wrap_the_access_path() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT name FROM users WHERE age > 30 ORDER BY name DESC LIMIT 10 OFFSET 5"
        ),
        [
            "LIMIT 10 OFFSET 5",
            "  SORT users.name DESC",
            "    external merge sort, spills to disk over the query budget",
            "    SCAN users",
            "      filter: users.age > 30",
        ]
    );
}

#[test]
fn indexed_join_reports_an_index_nested_loop() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT u.name, o.total FROM users u \
             JOIN orders o ON o.user_id = u.id WHERE u.age > 30 AND o.total > 100"
        ),
        [
            "JOIN INNER (index nested-loop)",
            "  on: u.id = o.user_id",
            "  streamed: no joined rows are materialized",
            "  SCAN u",
            "    filter: u.age > 30",
            "  INDEX PROBE o.user_id = u.id",
            "    filter: o.total > 100",
        ]
    );
}

#[test]
fn unindexed_join_column_falls_back_to_the_hash_join() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT u.name, o.total FROM users u JOIN orders o ON o.status = u.city"
        ),
        [
            "JOIN INNER (grace hash join)",
            "  on: u.city = o.status",
            "  SCAN u",
            "  SCAN o",
        ]
    );
}

#[test]
fn right_join_always_takes_the_hash_path() {
    let (_d, db) = seeded();
    // orders.user_id is indexed, but RIGHT JOIN preserves the new side and the
    // index nested-loop cannot do that.
    let lines = plan(
        &db,
        "EXPLAIN SELECT u.name, o.total FROM users u RIGHT JOIN orders o ON o.user_id = u.id",
    );
    assert_eq!(lines[0], "JOIN RIGHT (grace hash join)");
}

#[test]
fn aggregates_report_grouping_having_and_output_ordering() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT city, count(*), sum(age) FROM users WHERE age > 18 \
             GROUP BY city HAVING count(*) > 2 ORDER BY city LIMIT 3"
        ),
        [
            "LIMIT 3",
            "  SORT city ASC",
            "    external merge sort, spills to disk over the query budget",
            "    GROUP BY users.city",
            "      aggregates: count(*), sum(users.age)",
            "      having: count(*) > 2",
            "      SCAN users",
            "        filter: users.age > 18",
        ]
    );
    assert_eq!(
        plan(&db, "EXPLAIN SELECT count(*) FROM users"),
        [
            "AGGREGATE (single group)",
            "  aggregates: count(*)",
            "  SCAN users",
        ]
    );
}

#[test]
fn a_join_under_an_aggregate_is_not_the_streamed_path() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT u.city, count(*) FROM users u \
             JOIN orders o ON o.user_id = u.id GROUP BY u.city"
        ),
        [
            "GROUP BY u.city",
            "  aggregates: count(*)",
            "  JOIN INNER (index nested-loop)",
            "    on: u.id = o.user_id",
            "    SCAN u",
            "    INDEX PROBE o.user_id = u.id",
        ]
    );
}

#[test]
fn cross_table_predicates_are_evaluated_above_the_join() {
    let (_d, db) = seeded();
    assert_eq!(
        plan(
            &db,
            "EXPLAIN SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id \
             WHERE u.name = o.status"
        ),
        [
            "filter: u.name = o.status",
            "JOIN INNER (index nested-loop)",
            "  on: u.id = o.user_id",
            "  streamed: no joined rows are materialized",
            "  SCAN u",
            "  INDEX PROBE o.user_id = u.id",
        ]
    );
}

#[test]
fn bound_parameters_appear_resolved_in_the_plan() {
    let (_d, db) = seeded();
    let out = db
        .query_params(
            "EXPLAIN SELECT * FROM users WHERE email = ?",
            &[Value::Text("ada@example.com".into())],
        )
        .unwrap();
    let QueryOutput::Rows { rows, .. } = out else {
        panic!("expected rows");
    };
    assert_eq!(
        rows[0][0],
        Value::Text("INDEX LOOKUP users.email = 'ada@example.com'".into())
    );
}

#[test]
fn long_text_values_are_elided() {
    let (_d, db) = seeded();
    let long = "x".repeat(80);
    let lines = plan(
        &db,
        &format!("EXPLAIN SELECT * FROM users WHERE email = '{long}'"),
    );
    assert_eq!(
        lines[0],
        format!("INDEX LOOKUP users.email = '{}...'", "x".repeat(32))
    );
}

#[test]
fn explain_validates_the_query_instead_of_printing_a_plan_for_it() {
    let (_d, db) = seeded();
    assert!(error(&db, "EXPLAIN SELECT * FROM missing").contains("table not found"));
    assert!(error(&db, "EXPLAIN SELECT nope FROM users").contains("unknown column 'nope'"));
    assert!(
        error(&db, "EXPLAIN SELECT city, count(*) FROM users").contains("must appear in GROUP BY")
    );
    assert!(error(
        &db,
        "EXPLAIN SELECT * FROM users u JOIN users u ON u.id = u.id"
    )
    .contains("duplicate table alias"));
}

#[test]
fn explain_covers_only_select() {
    let (_d, db) = seeded();
    assert!(error(&db, "EXPLAIN UPDATE users SET age = 1")
        .contains("EXPLAIN is only supported for SELECT"));
    assert!(
        error(&db, "EXPLAIN DELETE FROM users").contains("EXPLAIN is only supported for SELECT")
    );
    assert!(error(&db, "EXPLAIN ANALYZE SELECT * FROM users")
        .contains("EXPLAIN ANALYZE is not supported"));
}

#[test]
fn explain_does_not_run_the_query() {
    let (_d, db) = seeded();
    db.query("INSERT INTO users (name, email) VALUES ('ada', 'ada@example.com')")
        .unwrap();
    // A plan for a DELETE-shaped read must leave the data alone, and planning
    // a scan must not consume it either.
    plan(&db, "EXPLAIN SELECT * FROM users");
    let QueryOutput::Rows { rows, .. } = db.query("SELECT name FROM users").unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 1);
}

/// The plan must describe the run that follows it: every access path EXPLAIN
/// names is exercised here against real data.
#[test]
fn planned_paths_return_the_same_rows_they_promise() {
    let (_d, db) = seeded();
    db.query(
        "INSERT INTO users (name, email, age, city) VALUES \
         ('ada', 'ada@example.com', 36, 'madrid'), \
         ('bob', 'bob@example.com', 24, 'madrid'), \
         ('eva', 'eva@example.com', 41, 'lisboa')",
    )
    .unwrap();

    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT name FROM users WHERE email = 'eva@example.com'")
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(rows, vec![vec![Value::Text("eva".into())]]);

    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT name FROM users WHERE city = 'madrid' ORDER BY name")
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("ada".into())],
            vec![Value::Text("bob".into())]
        ]
    );

    // The NO ACCESS path must agree with SQL's NULL semantics.
    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT name FROM users WHERE city = NULL")
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert!(rows.is_empty());
}
