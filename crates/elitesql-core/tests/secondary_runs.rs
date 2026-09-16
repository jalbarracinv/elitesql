use std::collections::BTreeSet;
use std::fs;

use elitesql_core::{Column, ColumnType, Db, DbOptions, Record, TableSchema, Value};

fn record(id: &str, group: &str, value: i64) -> Record {
    let mut record = Record::new();
    record.insert("id", Value::Text(id.into()));
    record.insert("group", Value::Text(group.into()));
    record.insert("value", Value::Int64(value));
    record
}

fn indexed_ids(db: &Db, group: &str) -> BTreeSet<String> {
    db.find_eq("items", "group", &Value::Text(group.into()))
        .unwrap()
        .into_iter()
        .map(|(id, row)| {
            assert_eq!(row["group"], Value::Text(group.into()));
            id
        })
        .collect()
}

fn canonical_ids(db: &Db, group: &str) -> BTreeSet<String> {
    db.scan("items")
        .unwrap()
        .into_iter()
        .filter_map(|(id, row)| (row["group"] == Value::Text(group.into())).then_some(id))
        .collect()
}

#[test]
fn equality_deltas_promote_without_resurrecting_old_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secondary-levels.esql");
    let options = DbOptions {
        memtable_max_bytes: u64::MAX,
        ..DbOptions::default()
    };
    let db = Db::create_with(&path, options.clone()).unwrap();
    db.create_table(TableSchema::new(
        "items",
        vec![
            Column::new("group", ColumnType::Text),
            Column::new("value", ColumnType::Int64),
        ],
    ))
    .unwrap();
    db.create_index("items", "group", false).unwrap();

    let mut checkpoint_bytes = Vec::new();
    let mut previous_bytes = 0;
    for batch in 0..24 {
        let mut txn = db.begin();
        for row in 0..40 {
            let id = format!("id-{batch:03}-{row:03}");
            let group = if row % 2 == 0 { "hot" } else { "cold" };
            txn.insert("items", record(&id, group, (batch * 40 + row) as i64))
                .unwrap();
        }
        if batch > 0 {
            let moved = format!("id-{:03}-000", batch - 1);
            let mut patch = Record::new();
            patch.insert("group", Value::Text("moved".into()));
            txn.update("items", &moved, patch).unwrap();
            let deleted = format!("id-{:03}-001", batch - 1);
            txn.delete("items", &deleted).unwrap();
        }
        txn.commit().unwrap();
        db.checkpoint().unwrap();
        let total = db.maintenance_stats().secondary_checkpoint_bytes_written;
        checkpoint_bytes.push(total - previous_bytes);
        previous_bytes = total;
    }
    db.wait_for_secondary_compaction().unwrap();

    assert_eq!(indexed_ids(&db, "hot"), canonical_ids(&db, "hot"));
    assert_eq!(indexed_ids(&db, "cold"), canonical_ids(&db, "cold"));
    assert_eq!(indexed_ids(&db, "moved"), canonical_ids(&db, "moved"));
    let smallest = *checkpoint_bytes
        .iter()
        .filter(|bytes| **bytes > 0)
        .min()
        .unwrap();
    let largest = *checkpoint_bytes.iter().max().unwrap();
    assert!(largest <= smallest * 3, "{checkpoint_bytes:?}");
    let stats = db.maintenance_stats();
    assert!(stats.secondary_run_compactions >= 2, "{stats:?}");
    assert!(stats.secondary_runs <= 12, "{stats:?}");
    assert!(stats.secondary_run_compaction_bytes_read > 0);
    assert!(stats.secondary_run_compaction_bytes_written > 0);
    drop(db);

    let reopened = Db::open_with(&path, options).unwrap();
    assert_eq!(
        indexed_ids(&reopened, "hot"),
        canonical_ids(&reopened, "hot")
    );
    assert_eq!(
        indexed_ids(&reopened, "moved"),
        canonical_ids(&reopened, "moved")
    );
}

#[test]
fn missing_secondary_level_is_rebuilt_from_canonical_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-secondary-run.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "items",
            vec![
                Column::new("group", ColumnType::Text),
                Column::new("value", ColumnType::Int64),
            ],
        ))
        .unwrap();
        db.create_index("items", "group", false).unwrap();
        for batch in 0..9 {
            let mut txn = db.begin();
            for row in 0..10 {
                let id = format!("id-{batch:03}-{row:03}");
                txn.insert("items", record(&id, "hot", (batch * 10 + row) as i64))
                    .unwrap();
            }
            txn.commit().unwrap();
            db.checkpoint().unwrap();
        }
        db.wait_for_secondary_compaction().unwrap();
    }

    let run = fs::read_dir(path.join("indexes"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("-L") && name.ends_with(".sidx.run"))
        })
        .expect("secondary level run exists");
    fs::remove_file(run).unwrap();

    let reopened = Db::open(&path).unwrap();
    assert_eq!(indexed_ids(&reopened, "hot").len(), 90);
    assert_eq!(
        indexed_ids(&reopened, "hot"),
        canonical_ids(&reopened, "hot")
    );
}

/// A row created and deleted between two publications never reaches a run, so
/// its removal needs no tombstone. Recording one anyway made every later
/// lookup of that key walk the whole history of it: with a single row left in
/// the group, three thousand create/delete cycles took one lookup from 8 us to
/// 132 us, and nothing bounded that growth.
///
/// The guard is a ratio measured inside one run of this test, not an absolute
/// time, so it reports the shape of the cost rather than the speed of the
/// machine: flat is about 1x, and the defect it replaces was above 6x here.
#[test]
fn churn_on_one_key_does_not_accumulate_tombstones() {
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("churn.esql")).unwrap();
    db.query(
        "CREATE TABLE items (id int AUTO_INCREMENT PRIMARY KEY, grp int NOT NULL, v int NOT NULL)",
    )
    .unwrap();
    db.query("CREATE INDEX ON items (grp)").unwrap();
    db.query("INSERT INTO items (grp, v) VALUES (1, 0)")
        .unwrap();
    // Publish a generation: only then does a removal consider a tombstone.
    db.checkpoint().unwrap();

    let live = |db: &Db| db.find_eq("items", "grp", &Value::Int64(1)).unwrap().len();
    let lookup_cost = |db: &Db| {
        let started = Instant::now();
        for _ in 0..200 {
            assert_eq!(live(db), 1);
        }
        started.elapsed()
    };

    let churn = |db: &Db, cycles: usize| {
        for _ in 0..cycles {
            let inserted = db
                .query("INSERT INTO items (grp, v) VALUES (1, 1) RETURNING id")
                .unwrap();
            let elitesql_core::QueryOutput::Rows { rows, .. } = inserted else {
                panic!("RETURNING yields rows")
            };
            db.query_params("DELETE FROM items WHERE id = ?", &[rows[0][0].clone()])
                .unwrap();
        }
    };

    churn(&db, 250);
    let early = lookup_cost(&db);
    churn(&db, 2_750);
    let late = lookup_cost(&db);
    assert_eq!(live(&db), 1, "the churn left exactly the seeded row");
    assert!(
        late < early * 4,
        "lookup cost grew with the churn: {early:?} after 250 cycles, {late:?} after 3000"
    );

    // A removal that does hide a published row is still recorded.
    db.query("INSERT INTO items (grp, v) VALUES (9, 9)")
        .unwrap();
    db.checkpoint().unwrap();
    let published = db.find_eq("items", "grp", &Value::Int64(9)).unwrap();
    assert_eq!(published.len(), 1);
    db.query_params(
        "DELETE FROM items WHERE id = ?",
        &[published[0].1["id"].clone()],
    )
    .unwrap();
    assert!(
        db.find_eq("items", "grp", &Value::Int64(9))
            .unwrap()
            .is_empty(),
        "deleting a published row must still hide it"
    );
    assert_eq!(live(&db), 1, "the other group is untouched");
}
