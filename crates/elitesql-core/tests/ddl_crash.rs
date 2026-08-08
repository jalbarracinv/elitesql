//! Crash injection with real process kills (SIGKILL) *during DDL*.
//!
//! The parent re-spawns this test binary as a worker that runs schema changes
//! in a tight loop over a table with a fixed set of records: rename the table,
//! rename its value column, add a column with a default, drop it again. The
//! parent kills the worker at a random point, reopens the database and checks
//! that the schema change was either not applied or fully applied — never half
//! of it, and never at the cost of a record.
//!
//! Override the round count with ELITESQL_DDL_CRASH_ITERS (default 12).

use std::time::Duration;

use elitesql_core::{
    check, Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value,
};

const ENV_WORKER: &str = "ELITESQL_DDL_CRASH_WORKER_DIR";
const RECORDS: i64 = 40;
const TMP_DEFAULT: i64 = 7;

fn worker_opts() -> DbOptions {
    DbOptions {
        durability: Durability::Safe,
        memtable_max_bytes: 32 * 1024,
        ..DbOptions::default()
    }
}

/// Either name the table may carry, and either name its value column may
/// carry: a rename swaps them, so both halves of every cycle are legal states.
const TABLES: [&str; 2] = ["t", "u"];
const COLUMNS: [&str; 2] = ["a", "b"];

fn current_table(db: &Db) -> Option<String> {
    db.tables()
        .into_iter()
        .find(|t| TABLES.contains(&t.as_str()))
}

fn value_column(db: &Db, table: &str) -> Option<String> {
    let schema = db.table_schema(table)?;
    schema
        .columns
        .iter()
        .map(|c| c.name.clone())
        .find(|n| COLUMNS.contains(&n.as_str()))
}

/// Worker mode: runs only when spawned by the parent with the env var set.
#[test]
fn ddl_crash_worker() {
    let Ok(dir) = std::env::var(ENV_WORKER) else {
        return; // normal test runs skip this
    };
    let db = Db::open_or_create_with(&dir, worker_opts()).unwrap();
    if current_table(&db).is_none() {
        db.create_table(TableSchema::new(
            "t",
            vec![Column::new("a", ColumnType::Int64).not_null()],
        ))
        .unwrap();
        let mut txn = db.begin();
        for n in 0..RECORDS {
            let mut rec = Record::new();
            rec.insert("id".into(), Value::Text(format!("R-{n:04}")));
            rec.insert("a".into(), Value::Int64(n));
            txn.insert("t", rec).unwrap();
        }
        txn.commit().unwrap();
        db.checkpoint().unwrap();
    }

    loop {
        // Read the state fresh every round: the previous process may have been
        // killed anywhere, and its DDL completed by recovery on open.
        let table = current_table(&db).expect("one of the two names always exists");
        let column = value_column(&db, &table).expect("the value column always exists");
        if db
            .table_schema(&table)
            .is_some_and(|s| s.column("tmp").is_some())
        {
            db.drop_column(&table, "tmp").unwrap();
        }

        let other_table = if table == TABLES[0] {
            TABLES[1]
        } else {
            TABLES[0]
        };
        db.rename_table(&table, other_table).unwrap();

        let other_column = if column == COLUMNS[0] {
            COLUMNS[1]
        } else {
            COLUMNS[0]
        };
        db.rename_column(other_table, &column, other_column)
            .unwrap();

        db.add_column(
            other_table,
            Column::new("tmp", ColumnType::Int64)
                .not_null()
                .with_default(&Value::Int64(TMP_DEFAULT)),
        )
        .unwrap();
        db.drop_column(other_table, "tmp").unwrap();
    }
}

#[test]
fn kill9_during_ddl_leaves_a_consistent_schema() {
    if std::env::var(ENV_WORKER).is_ok() {
        return; // don't recurse inside the worker process
    }
    let iterations: u32 = std::env::var("ELITESQL_DDL_CRASH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);

    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("ddl-crash.esql");
    let exe = std::env::current_exe().unwrap();
    let mut rng: u64 = 0x0BAD_C0DE_F00D_1234;

    for round in 0..iterations {
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "ddl_crash_worker", "--nocapture"])
            .env(ENV_WORKER, db_dir.as_os_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(25 + (rng % 200)));
        child.kill().unwrap(); // SIGKILL on unix
        child.wait().unwrap();

        // 1. The database opens, replaying any interrupted schema change.
        let db = Db::open_with(&db_dir, worker_opts())
            .unwrap_or_else(|e| panic!("round {round}: recovery failed: {e}"));

        // 2. Exactly one of the two table names exists: a rename never leaves
        //    the table under both names, nor loses it entirely.
        let live: Vec<String> = db
            .tables()
            .into_iter()
            .filter(|t| TABLES.contains(&t.as_str()))
            .collect();
        assert_eq!(live.len(), 1, "round {round}: tables = {:?}", db.tables());
        let table = &live[0];

        // 3. Exactly one of the two column names exists, for the same reason.
        let schema = db.table_schema(table).unwrap();
        let value_columns: Vec<&str> = schema
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| COLUMNS.contains(n))
            .collect();
        assert_eq!(
            value_columns.len(),
            1,
            "round {round}: columns = {:?}",
            schema.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        let column = value_columns[0];

        // 4. Every record survived every rewrite, with its value intact.
        let rows = db.scan(table).unwrap();
        assert_eq!(
            rows.len() as i64,
            RECORDS,
            "round {round}: {} records left in {table}",
            rows.len()
        );
        for (id, record) in &rows {
            let n: i64 = id.trim_start_matches("R-").parse().unwrap();
            assert_eq!(
                record.get(column),
                Some(&Value::Int64(n)),
                "round {round}: {id} lost its value"
            );
        }

        // 5. An interrupted ADD COLUMN is either absent or complete: if the
        //    column is there it is backfilled everywhere and NOT NULL, never a
        //    schema its own records violate.
        if let Some(tmp_col) = schema.column("tmp") {
            assert!(
                !tmp_col.nullable,
                "round {round}: NOT NULL was published before the backfill finished"
            );
            for (id, record) in &rows {
                assert_eq!(
                    record.get("tmp"),
                    Some(&Value::Int64(TMP_DEFAULT)),
                    "round {round}: {id} was not backfilled"
                );
            }
        }

        // 6. Writes still work on whatever state we recovered into.
        let mut rec = Record::new();
        rec.insert("id".into(), Value::Text(format!("probe-{round}")));
        rec.insert(column.to_owned(), Value::Int64(-1));
        db.insert(table, rec).unwrap();
        db.delete(table, &format!("probe-{round}")).unwrap();

        drop(db); // release the lock for the next worker
    }

    let report = check(&db_dir).unwrap();
    assert!(report.is_ok(), "post-crash check: {:?}", report.errors);
}
