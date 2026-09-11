//! Compare the common INTEGER/REAL SQL subset with SQLite. Physical ids,
//! timestamps, NaN and signed-zero ordering are engine-specific and excluded.
use elitesql_core::{Db, DbOptions, MemoryOptions, QueryOutput, Value};
use rusqlite::{types::ValueRef, Connection};

fn compare(db: &Db, sqlite: &Connection, sql: &str) {
    let QueryOutput::Rows { rows, .. } = db.query(sql).unwrap() else {
        panic!("expected rows: {sql}");
    };
    let mut statement = sqlite.prepare(sql).unwrap();
    let columns = statement.column_count();
    let expected: Vec<Vec<Value>> = statement
        .query_map([], |row| {
            Ok((0..columns)
                .map(|column| match row.get_ref(column).unwrap() {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => Value::Int64(n),
                    ValueRef::Real(n) => Value::Float64(n),
                    _ => panic!("unexpected fixture type"),
                })
                .collect())
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, expected, "{sql}");
}

#[test]
fn exact_numbers_match_sqlite_across_join_plans_and_spill() {
    for budget in [2048, 1024 * 1024] {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::create_with(
            directory.path().join("db"),
            DbOptions {
                memory: MemoryOptions {
                    query_working_bytes: budget,
                    spill_directory: Some(directory.path().join("spill")),
                    ..MemoryOptions::default()
                },
                ..DbOptions::default()
            },
        )
        .unwrap();
        let sqlite = Connection::open_in_memory().unwrap();
        db.query("CREATE TABLE ints(n int)").unwrap();
        db.query("CREATE TABLE floats(n float64)").unwrap();
        sqlite
            .execute_batch("CREATE TABLE ints(n INTEGER); CREATE TABLE floats(n REAL)")
            .unwrap();
        // Duplicates and unmatched/NULL rows exercise outer joins and run merges.
        for _ in 0..4 {
            for n in [
                None,
                Some(i64::MIN),
                Some(-9007199254740993),
                Some(-1),
                Some(0),
                Some(1),
                Some(9007199254740992),
                Some(9007199254740993),
                Some(i64::MAX),
            ] {
                db.query_params(
                    "INSERT INTO ints(n) VALUES(?)",
                    &[n.map_or(Value::Null, Value::Int64)],
                )
                .unwrap();
                sqlite
                    .execute("INSERT INTO ints(n) VALUES(?)", [n])
                    .unwrap();
            }
            for n in [
                None,
                Some(-1.5),
                Some(-1.0),
                Some(0.0),
                Some(1.0),
                Some(9007199254740992.0),
                Some(9223372036854775808.0),
            ] {
                db.query_params(
                    "INSERT INTO floats(n) VALUES(?)",
                    &[n.map_or(Value::Null, Value::Float64)],
                )
                .unwrap();
                sqlite
                    .execute("INSERT INTO floats(n) VALUES(?)", [n])
                    .unwrap();
            }
        }
        for index in [None, Some("ints"), Some("floats")] {
            if let Some(table) = index {
                db.query(&format!("CREATE INDEX ON {table}(n)")).unwrap();
            }
            for sql in [
                "SELECT n FROM ints ORDER BY n",
                "SELECT n FROM ints ORDER BY n DESC LIMIT 7 OFFSET 3",
                "SELECT MIN(n), MAX(n) FROM ints",
                "SELECT n FROM ints WHERE n = 9007199254740993 ORDER BY n",
                "SELECT n FROM ints WHERE n < 9223372036854775808.0 ORDER BY n",
                "SELECT i.n,f.n FROM ints i JOIN floats f ON i.n = f.n ORDER BY i.n,f.n",
                "SELECT i.n,f.n FROM ints i LEFT JOIN floats f ON i.n = f.n ORDER BY i.n,f.n",
                "SELECT i.n,f.n FROM ints i RIGHT JOIN floats f ON i.n = f.n ORDER BY i.n,f.n",
            ] {
                compare(&db, &sqlite, sql);
            }
            if let Some(table) = index {
                db.query(&format!("DROP INDEX ON {table}(n)")).unwrap();
            }
        }
        if budget == 2048 {
            assert!(db.query_memory_stats().spill_files > 0);
        }
    }
}
