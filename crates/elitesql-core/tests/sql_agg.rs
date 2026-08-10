//! Phase 2.5: aggregates (COUNT/SUM/AVG/MIN/MAX), GROUP BY and HAVING,
//! including SQL NULL semantics.

use elitesql_core::{Db, Error, QueryOutput, Value};
use tempfile::TempDir;

fn seeded() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("agg.esql")).unwrap();
    db.query("CREATE TABLE sales (region text NOT NULL, rep text, amount int64, score float64)")
        .unwrap();
    db.query(
        "INSERT INTO sales (region, rep, amount, score) VALUES \
         ('north', 'ana', 100, 1.5), \
         ('north', 'bob', 200, 2.5), \
         ('north', NULL, NULL, NULL), \
         ('south', 'eva', 50, 4.0), \
         ('south', 'gil', 350, NULL), \
         ('west', 'ana', 75, 3.0)",
    )
    .unwrap();
    (dir, db)
}

fn rows(out: QueryOutput) -> (Vec<String>, Vec<Vec<Value>>) {
    match out {
        QueryOutput::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn global_aggregates() {
    let (_d, db) = seeded();
    let (cols, r) = rows(
        db.query(
            "SELECT count(*), count(amount), sum(amount), avg(amount), min(amount), max(amount) FROM sales",
        )
        .unwrap(),
    );
    assert_eq!(
        cols,
        vec![
            "count(*)",
            "count(amount)",
            "sum(amount)",
            "avg(amount)",
            "min(amount)",
            "max(amount)"
        ]
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int64(6), "COUNT(*) counts all rows");
    assert_eq!(r[0][1], Value::Int64(5), "COUNT(col) ignores NULLs");
    assert_eq!(r[0][2], Value::Int64(775));
    assert_eq!(
        r[0][3],
        Value::Float64(155.0),
        "AVG over non-null values only"
    );
    assert_eq!(r[0][4], Value::Int64(50));
    assert_eq!(r[0][5], Value::Int64(350));
}

#[test]
fn aggregates_over_empty_set() {
    let (_d, db) = seeded();
    let (_, r) = rows(
        db.query(
            "SELECT count(*), sum(amount), avg(amount), min(amount) FROM sales WHERE amount > 9999",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1, "global aggregate always yields one row");
    assert_eq!(r[0][0], Value::Int64(0));
    assert_eq!(r[0][1], Value::Null, "SUM of empty set is NULL");
    assert_eq!(r[0][2], Value::Null, "AVG of empty set is NULL");
    assert_eq!(r[0][3], Value::Null, "MIN of empty set is NULL");
}

#[test]
fn sum_promotes_to_float_when_mixed() {
    let (_d, db) = seeded();
    let (_, r) = rows(db.query("SELECT sum(score) FROM sales").unwrap());
    assert_eq!(r[0][0], Value::Float64(11.0));
    let (_, r) = rows(db.query("SELECT avg(score) FROM sales").unwrap());
    assert_eq!(
        r[0][0],
        Value::Float64(2.75),
        "AVG ignores the two NULL scores"
    );
}

#[test]
fn group_by_with_order_and_aliases() {
    let (_d, db) = seeded();
    let (cols, r) = rows(
        db.query(
            "SELECT region, count(*) AS n, sum(amount) AS total FROM sales \
             GROUP BY region ORDER BY total DESC",
        )
        .unwrap(),
    );
    assert_eq!(cols, vec!["region", "n", "total"]);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("south".into()));
    assert_eq!(r[0][1], Value::Int64(2));
    assert_eq!(r[0][2], Value::Int64(400));
    assert_eq!(r[1][0], Value::Text("north".into()));
    assert_eq!(r[1][2], Value::Int64(300));
    assert_eq!(r[2][0], Value::Text("west".into()));
    assert_eq!(r[2][2], Value::Int64(75));
}

#[test]
fn group_by_multiple_columns_and_null_groups() {
    let (_d, db) = seeded();
    // NULL rep forms its own group (SQL GROUP BY semantics).
    let (_, r) = rows(
        db.query("SELECT rep, count(*) AS n FROM sales GROUP BY rep ORDER BY n DESC, rep ASC")
            .unwrap(),
    );
    // ana appears twice; bob/eva/gil/NULL once each.
    assert_eq!(r.len(), 5);
    assert_eq!(r[0][0], Value::Text("ana".into()));
    assert_eq!(r[0][1], Value::Int64(2));
    assert!(r
        .iter()
        .any(|row| row[0] == Value::Null && row[1] == Value::Int64(1)));

    let (_, r) = rows(
        db.query("SELECT region, rep, count(*) AS n FROM sales GROUP BY region, rep")
            .unwrap(),
    );
    assert_eq!(r.len(), 6, "every (region, rep) pair is distinct here");
}

#[test]
fn having_filters_groups() {
    let (_d, db) = seeded();
    let (_, r) = rows(
        db.query(
            "SELECT region, sum(amount) AS total FROM sales \
             GROUP BY region HAVING sum(amount) >= 300 ORDER BY region",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("north".into()));
    assert_eq!(r[1][0], Value::Text("south".into()));

    // HAVING can combine aggregates and grouped columns.
    let (_, r) = rows(
        db.query(
            "SELECT region, count(*) AS n FROM sales \
             GROUP BY region HAVING count(*) > 1 AND region <> 'south'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("north".into()));

    // HAVING an aggregate that is not in the SELECT list.
    let (_, r) = rows(
        db.query("SELECT region FROM sales GROUP BY region HAVING max(amount) = 350")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("south".into()));

    // Global HAVING without GROUP BY.
    let (_, r) = rows(
        db.query("SELECT count(*) FROM sales HAVING count(*) > 100")
            .unwrap(),
    );
    assert!(r.is_empty());
}

#[test]
fn group_by_without_aggregates_is_distinct_groups() {
    let (_d, db) = seeded();
    let (_, r) = rows(
        db.query("SELECT region FROM sales GROUP BY region ORDER BY region")
            .unwrap(),
    );
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("north".into()));
}

#[test]
fn aggregates_compose_with_where_and_joins() {
    let (_d, db) = seeded();
    db.query("CREATE TABLE regions (name text NOT NULL, country text)")
        .unwrap();
    db.query(
        "INSERT INTO regions (id, name, country) VALUES \
         ('r1', 'north', 'peru'), ('r2', 'south', 'peru'), ('r3', 'west', 'chile')",
    )
    .unwrap();

    let (_, r) = rows(
        db.query(
            "SELECT g.country, sum(s.amount) AS total FROM sales s \
             JOIN regions g ON g.name = s.region \
             WHERE s.amount > 60 \
             GROUP BY g.country ORDER BY total DESC",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("peru".into()));
    assert_eq!(r[0][1], Value::Int64(650), "100+200+350, WHERE filtered 50");
    assert_eq!(r[1][0], Value::Text("chile".into()));
    assert_eq!(r[1][1], Value::Int64(75));
}

#[test]
fn count_star_with_limit_offset() {
    let (_d, db) = seeded();
    let (_, r) = rows(
        db.query("SELECT region, count(*) AS n FROM sales GROUP BY region ORDER BY region LIMIT 2 OFFSET 1")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("south".into()));
    assert_eq!(r[1][0], Value::Text("west".into()));
}

#[test]
fn aggregate_errors_are_clear() {
    let (_d, db) = seeded();
    let err = |sql: &str, needle: &str| match db.query(sql) {
        Err(Error::Sql(m)) => assert!(
            m.to_lowercase().contains(&needle.to_lowercase()),
            "for {sql}: got {m:?}"
        ),
        other => panic!("for {sql}: expected Sql error, got {other:?}"),
    };
    err(
        "SELECT region, count(*) FROM sales",
        "must appear in GROUP BY",
    );
    err(
        "SELECT * FROM sales GROUP BY region",
        "list columns explicitly",
    );
    err(
        "SELECT sum(region) FROM sales",
        "requires an int64 or float64",
    );
    err("SELECT avg(rep) FROM sales", "requires an int64 or float64");
    err(
        "SELECT count(*) FROM sales WHERE count(*) > 1",
        "SELECT list and HAVING",
    );
    err(
        "SELECT region FROM sales GROUP BY region HAVING rep = 'ana'",
        "is not grouped",
    );
    err(
        "SELECT region, count(*) FROM sales GROUP BY region ORDER BY count(*)",
        "alias",
    );
    let (_, distinct) = rows(db.query("SELECT count(DISTINCT rep) FROM sales").unwrap());
    assert_eq!(distinct, vec![vec![Value::Int64(4)]]);
    err("SELECT sum(*) FROM sales", "only COUNT accepts *");
}

#[test]
fn sum_overflow_is_an_error_not_a_wrap() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("ovf.esql")).unwrap();
    db.query("CREATE TABLE t (n int64)").unwrap();
    db.query(&format!(
        "INSERT INTO t (n) VALUES ({}), ({})",
        i64::MAX,
        i64::MAX
    ))
    .unwrap();
    let err = db.query("SELECT sum(n) FROM t").unwrap_err();
    assert!(err.to_string().contains("overflow"), "{err}");
}
