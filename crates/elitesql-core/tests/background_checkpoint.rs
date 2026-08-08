use elitesql_core::{Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        "events",
        vec![
            Column::new("payload", ColumnType::Text).not_null(),
            Column::new("generation", ColumnType::Int64).not_null(),
        ],
    )
}

fn record(id: &str, payload: &str, generation: i64) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(id.into()));
    record.insert("payload".into(), Value::Text(payload.into()));
    record.insert("generation".into(), Value::Int64(generation));
    record
}

#[test]
fn frozen_memtable_stays_queryable_while_wal_tail_keeps_growing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("background.esql");
    let options = DbOptions {
        durability: Durability::Fast,
        memtable_max_bytes: 2 * 1024 * 1024,
        ..DbOptions::default()
    };

    {
        let db = Db::create_with(&path, options.clone()).unwrap();
        db.create_table(schema()).unwrap();
        let payload = "x".repeat(1024);

        // One commit crosses the threshold and hands its complete generation
        // to the background writer.
        let mut initial = db.begin();
        for n in 0..3_000 {
            initial
                .insert("events", record(&format!("e-{n:05}"), &payload, 0))
                .unwrap();
        }
        initial.commit().unwrap();

        let memory = db.global_memory_stats();
        assert_eq!(
            memory.maintenance_in_use_bytes, memory.maintenance_capacity_bytes,
            "the frozen generation owns the bounded maintenance reservation"
        );
        assert_eq!(
            db.get("events", "e-00042").unwrap().unwrap()["generation"],
            Value::Int64(0),
            "reads must merge the frozen and active generations"
        );

        // These commits land after the frozen WAL boundary. Publication must
        // copy them into the new WAL, and a newer active value must win over
        // the same key in the frozen segment.
        let mut tail = db.begin();
        let mut patch = Record::new();
        patch.insert("payload".into(), Value::Text("newer".into()));
        patch.insert("generation".into(), Value::Int64(1));
        tail.update("events", "e-00042", patch).unwrap();
        for n in 3_000..3_100 {
            tail.insert("events", record(&format!("e-{n:05}"), "tail", 1))
                .unwrap();
        }
        tail.commit().unwrap();

        // Explicit checkpoint is a barrier: it waits for the frozen publish,
        // then drains the active WAL tail synchronously.
        db.checkpoint().unwrap();
        assert_eq!(db.scan("events").unwrap().len(), 3_100);
    }

    let reopened = Db::open_with(&path, options).unwrap();
    assert_eq!(reopened.scan("events").unwrap().len(), 3_100);
    let updated = reopened.get("events", "e-00042").unwrap().unwrap();
    assert_eq!(updated["generation"], Value::Int64(1));
    assert_eq!(updated["payload"], Value::Text("newer".into()));
    assert!(reopened.get("events", "e-03099").unwrap().is_some());
}
