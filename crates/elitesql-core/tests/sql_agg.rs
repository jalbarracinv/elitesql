//! Phase 2.5: aggregates (COUNT/SUM/AVG/MIN/MAX), GROUP BY and HAVING,
//! including SQL NULL semantics.

use elitesql_core::{Db, DbOptions, Error, QueryOutput, Record, Value};
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

// --- Hash aggregation and its bounded fallback ------------------------------------

/// The same 3,000-row dataset in a database with the given per-query budget.
/// A budget too small for the group table forces the external sort-merge
/// fallback, so both strategies can be compared row for row.
fn grouped_dataset(query_working_bytes: usize) -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let mut options = DbOptions::default();
    options.memory.query_working_bytes = query_working_bytes;
    let db = Db::create_with(dir.path().join("agg.esql"), options).unwrap();
    db.query("CREATE TABLE sales (region text NOT NULL, rep text, amount int64, score float64)")
        .unwrap();
    let mut txn = db.begin();
    for i in 0..3_000usize {
        let mut record = Record::new();
        record.insert("region".into(), Value::Text(format!("r{:03}", i % 700)));
        if i % 11 != 0 {
            record.insert("rep".into(), Value::Text(format!("p{:02}", i % 37)));
        }
        record.insert("amount".into(), Value::Int64(((i * 7_919) % 1_000) as i64));
        if i % 5 != 0 {
            record.insert("score".into(), Value::Float64((i % 13) as f64 / 2.0));
        }
        txn.insert("sales", record).unwrap();
    }
    txn.commit().unwrap();
    (dir, db)
}

const GROUPED_QUERIES: [&str; 5] = [
    "SELECT region, count(*), count(rep), sum(amount), avg(score), min(rep), max(amount) \
     FROM sales GROUP BY region",
    "SELECT region, count(distinct rep) AS reps, sum(amount) AS total FROM sales \
     GROUP BY region HAVING count(*) > 3 ORDER BY reps DESC, region LIMIT 50 OFFSET 5",
    "SELECT rep, count(*) AS n, avg(amount) FROM sales WHERE amount > 500 GROUP BY rep ORDER BY rep",
    "SELECT region, rep, count(*) FROM sales GROUP BY region, rep",
    "SELECT rep FROM sales GROUP BY rep",
];

#[test]
fn hash_aggregation_and_sort_fallback_agree_row_for_row() {
    let (_default_dir, hashed) = grouped_dataset(16 * 1024 * 1024);
    // 8 KiB cannot hold 700 groups, so every grouped query below abandons
    // the hash table and re-runs as a spilling sort-merge.
    let (_tiny_dir, sorted) = grouped_dataset(8 * 1024);
    for sql in GROUPED_QUERIES {
        let (hashed_columns, hashed_rows) = rows(hashed.query(sql).unwrap());
        let (sorted_columns, sorted_rows) = rows(sorted.query(sql).unwrap());
        assert_eq!(hashed_columns, sorted_columns, "{sql}");
        assert_eq!(hashed_rows, sorted_rows, "{sql}");
    }
}

#[test]
fn hash_aggregation_counts_match_an_independent_tally() {
    let (_dir, db) = grouped_dataset(16 * 1024 * 1024);
    let mut expected: std::collections::BTreeMap<String, (i64, i64)> =
        std::collections::BTreeMap::new();
    for i in 0..3_000usize {
        let entry = expected.entry(format!("r{:03}", i % 700)).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ((i * 7_919) % 1_000) as i64;
    }
    let (_, result) = rows(
        db.query("SELECT region, count(*), sum(amount) FROM sales GROUP BY region ORDER BY region")
            .unwrap(),
    );
    assert_eq!(result.len(), 700);
    for (row, (region, (count, total))) in result.iter().zip(&expected) {
        assert_eq!(row[0], Value::Text(region.clone()));
        assert_eq!(row[1], Value::Int64(*count));
        assert_eq!(row[2], Value::Int64(*total));
    }

    // Groups come out in first-seen order without ORDER BY, like the sort path.
    let (_, unordered) = rows(
        db.query("SELECT region FROM sales GROUP BY region LIMIT 3")
            .unwrap(),
    );
    assert_eq!(
        unordered,
        vec![
            vec![Value::Text("r000".into())],
            vec![Value::Text("r001".into())],
            vec![Value::Text("r002".into())],
        ]
    );
}

#[test]
fn count_distinct_per_group_respects_the_budget_fallback() {
    // COUNT(DISTINCT) keeps one set per group; the tiny budget must still
    // produce exact counts through the fallback.
    for budget in [16 * 1024 * 1024, 8 * 1024] {
        let (_dir, db) = grouped_dataset(budget);
        let (_, result) = rows(
            db.query(
                "SELECT rep, count(distinct region) AS regions, count(distinct amount) AS amounts \
                 FROM sales GROUP BY rep ORDER BY rep",
            )
            .unwrap(),
        );
        assert_eq!(result.len(), 38, "37 reps plus the NULL group");
        for row in &result {
            let Value::Int64(regions) = row[1] else {
                panic!("count is an integer");
            };
            let Value::Int64(amounts) = row[2] else {
                panic!("count is an integer");
            };
            assert!(regions > 0 && amounts > 0);
        }
        // The NULL rep group holds every row where i % 11 == 0 (273 rows).
        let null_group = result
            .iter()
            .find(|row| row[0].is_null())
            .expect("NULL group present");
        let (_, null_count) = rows(
            db.query("SELECT count(*) FROM sales WHERE rep IS NULL")
                .unwrap(),
        );
        assert_eq!(null_count[0][0], Value::Int64(273));
        assert!(matches!(null_group[1], Value::Int64(n) if n <= 273));
    }
}
