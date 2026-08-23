use std::sync::{Arc, Barrier};

use elitesql_core::{
    Column, ColumnType, Db, DbOptions, Durability, Error, Record, TableSchema, Value,
};

fn record(id: String, writer: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(id));
    record.insert("writer".into(), Value::Int64(writer as i64));
    record
}

#[test]
fn fast_and_balanced_coordinator_publish_distinct_versions_and_recover_every_frame() {
    const WRITERS: usize = 16;
    const COMMITS: usize = 20;

    for durability in [Durability::Fast, Durability::Balanced] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coordinated.esql");
        let db = Arc::new(
            Db::create_with(
                &path,
                DbOptions {
                    durability,
                    memtable_max_bytes: u64::MAX,
                    ..DbOptions::default()
                },
            )
            .unwrap(),
        );
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("writer", ColumnType::Int64).not_null()],
        ))
        .unwrap();

        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles = (0..WRITERS)
            .map(|writer| {
                let db = db.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut versions = Vec::with_capacity(COMMITS);
                    for commit in 0..COMMITS {
                        let mut transaction = db.begin();
                        transaction
                            .insert("docs", record(format!("{writer:02}-{commit:03}"), writer))
                            .unwrap();
                        barrier.wait();
                        versions.push(transaction.commit().unwrap());
                    }
                    versions
                })
            })
            .collect::<Vec<_>>();
        let mut versions = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        versions.sort_unstable();

        assert_eq!(versions.len(), WRITERS * COMMITS);
        assert!(versions.windows(2).all(|pair| pair[1] == pair[0] + 1));
        let stats = db.maintenance_stats();
        assert_eq!(stats.commits, (WRITERS * COMMITS) as u64);
        assert!(stats.coordinated_batches > 0, "no batch formed: {stats:?}");
        assert!(
            stats.coordinated_commits >= 2,
            "no commits shared coordinated publication: {stats:?}"
        );
        assert_eq!(db.scan("docs").unwrap().len(), WRITERS * COMMITS);
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(reopened.scan("docs").unwrap().len(), WRITERS * COMMITS);
    }
}

#[test]
fn fast_coordinator_falls_back_to_conflict_checks_for_duplicate_ids() {
    const WRITERS: usize = 16;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::create_with(
            dir.path().join("conflict.esql"),
            DbOptions {
                durability: Durability::Fast,
                ..DbOptions::default()
            },
        )
        .unwrap(),
    );
    db.create_table(TableSchema::new(
        "docs",
        vec![Column::new("writer", ColumnType::Int64).not_null()],
    ))
    .unwrap();

    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles = (0..WRITERS)
        .map(|writer| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut transaction = db.begin();
                transaction
                    .insert("docs", record("same".into(), writer))
                    .unwrap();
                barrier.wait();
                transaction.commit()
            })
        })
        .collect::<Vec<_>>();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(outcomes
        .iter()
        .filter(|result| result.is_err())
        .all(|result| matches!(result, Err(Error::Conflict(_)))));
    assert_eq!(db.scan("docs").unwrap().len(), 1);
}
