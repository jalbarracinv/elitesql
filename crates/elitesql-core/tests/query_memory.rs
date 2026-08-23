use elitesql_core::{
    Db, DbOptions, Error, MemoryOptions, QueryOutput, Record, Value, VectorIndexOptions,
    VectorSearchOptions,
};
use std::fs;
use std::sync::{mpsc, Arc};
use std::time::Duration;

fn rows(output: QueryOutput) -> Vec<Vec<Value>> {
    match output {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn ingest_performance_profile_is_valid_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let options = DbOptions::ingest_performance();
    assert_eq!(options.memtable_max_bytes, 128 * 1024 * 1024);
    assert_eq!(options.memory.total_memory_bytes, 512 * 1024 * 1024);
    assert_eq!(options.memory.index_delta_pool_bytes, 192 * 1024 * 1024);
    assert_eq!(options.memory.maintenance_pool_bytes, 192 * 1024 * 1024);

    let db = Db::create_with(dir.path().join("profile.esql"), options).unwrap();
    let stats = db.global_memory_stats();
    assert_eq!(stats.total_bytes, 512 * 1024 * 1024);
    assert_eq!(stats.index_delta_capacity_bytes, 192 * 1024 * 1024);
    assert_eq!(stats.maintenance_capacity_bytes, 192 * 1024 * 1024);
}

#[test]
fn default_profile_matches_the_measured_vector_restart_budget() {
    let options = DbOptions::default();
    assert_eq!(options.memtable_max_bytes, 64 * 1024 * 1024);
    assert_eq!(options.memory.total_memory_bytes, 384 * 1024 * 1024);
    assert_eq!(options.memory.index_delta_pool_bytes, 128 * 1024 * 1024);
    assert_eq!(options.memory.maintenance_pool_bytes, 128 * 1024 * 1024);
}

#[test]
fn order_by_spills_under_a_tiny_budget_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let spill_dir = dir.path().join("query-spill");
    let db = Db::create_with(
        dir.path().join("memory.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 512,
                scan_batch_rows: 4,
                spill_directory: Some(spill_dir.clone()),
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items (name text NOT NULL, score int64 NOT NULL)")
        .unwrap();
    for chunk in 0..10 {
        let values = (0..20)
            .map(|i| {
                let n = chunk * 20 + i;
                format!("('item-{n:03}', {})", 199 - n)
            })
            .collect::<Vec<_>>()
            .join(",");
        db.query(&format!("INSERT INTO items (name, score) VALUES {values}"))
            .unwrap();
    }

    let result = rows(
        db.query("SELECT name, score FROM items ORDER BY score DESC LIMIT 7 OFFSET 3")
            .unwrap(),
    );
    assert_eq!(result.len(), 7);
    assert_eq!(result[0][1], Value::Int64(196));
    assert_eq!(result[6][1], Value::Int64(190));

    let stats = db.query_memory_stats();
    assert!(stats.spill_files > 0, "tiny budget must exercise spill");
    assert!(stats.spilled_bytes > 0);
    assert!(stats.peak_buffer_bytes >= 512);
    assert!(
        spill_dir.read_dir().unwrap().next().is_none(),
        "temporary runs must be removed when the query finishes"
    );
}

#[test]
fn cursor_streams_batches_from_one_stable_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("cursor.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 4 * 1024,
                scan_batch_rows: 3,
                spill_directory: None,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items (n int64 NOT NULL)").unwrap();
    for n in 0..20 {
        db.query(&format!(
            "INSERT INTO items (id, n) VALUES ('r-{n:03}', {n})"
        ))
        .unwrap();
    }

    let mut cursor = db
        .query_cursor("SELECT id, n FROM items WHERE n >= 5 LIMIT 10")
        .unwrap();
    assert_eq!(cursor.columns(), ["id", "n"]);
    // The cursor owns a snapshot. A later id must not appear between batches.
    db.query("INSERT INTO items (id, n) VALUES ('r-999', 999)")
        .unwrap();

    let first = cursor.next_batch(4).unwrap();
    let second = cursor.next_batch(20).unwrap();
    assert_eq!(first.len(), 4);
    assert_eq!(second.len(), 6);
    assert_eq!(first[0][1], Value::Int64(5));
    assert_eq!(second[5][1], Value::Int64(14));
    assert!(cursor.next().is_none());
}

#[test]
fn high_cardinality_group_by_uses_bounded_sorted_runs() {
    let dir = tempfile::tempdir().unwrap();
    let spill_dir = dir.path().join("aggregate-spill");
    let db = Db::create_with(
        dir.path().join("groups.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 768,
                scan_batch_rows: 5,
                spill_directory: Some(spill_dir.clone()),
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE events (category text NOT NULL, amount int64 NOT NULL)")
        .unwrap();
    for n in 0..120 {
        db.query(&format!(
            "INSERT INTO events (id, category, amount) VALUES ('e-{n:03}', 'c-{:03}', 1)",
            n % 40
        ))
        .unwrap();
    }

    let result = rows(
        db.query("SELECT category, count(*) AS n FROM events GROUP BY category ORDER BY category")
            .unwrap(),
    );
    assert_eq!(result.len(), 40);
    assert_eq!(result[0], [Value::Text("c-000".into()), Value::Int64(3)]);
    assert_eq!(result[39], [Value::Text("c-039".into()), Value::Int64(3)]);
    assert!(db.query_memory_stats().spill_files > 0);
    assert!(spill_dir.read_dir().unwrap().next().is_none());
}

#[test]
fn count_distinct_spills_under_the_query_budget() {
    let dir = tempfile::tempdir().unwrap();
    let spill_dir = dir.path().join("distinct-spill");
    let db = Db::create_with(
        dir.path().join("distinct.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 512,
                scan_batch_rows: 4,
                spill_directory: Some(spill_dir.clone()),
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE events (actor text)").unwrap();
    for n in 0..160 {
        db.query(&format!(
            "INSERT INTO events (id, actor) VALUES ('e-{n:03}', 'actor-{:03}')",
            n % 80
        ))
        .unwrap();
    }
    db.query("INSERT INTO events (id, actor) VALUES ('null-row', NULL)")
        .unwrap();

    assert_eq!(
        rows(
            db.query("SELECT count(DISTINCT actor) AS actors FROM events")
                .unwrap()
        ),
        vec![vec![Value::Int64(80)]]
    );
    assert!(db.query_memory_stats().spill_files > 0);
    assert!(spill_dir.read_dir().unwrap().next().is_none());
}

#[test]
fn indexed_join_streams_probes_and_spills_only_the_sort() {
    let dir = tempfile::tempdir().unwrap();
    let spill_dir = dir.path().join("join-spill");
    let db = Db::create_with(
        dir.path().join("join.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 640,
                scan_batch_rows: 4,
                spill_directory: Some(spill_dir.clone()),
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE users (name text NOT NULL)").unwrap();
    db.query("CREATE TABLE orders (user_id text NOT NULL, score int64 NOT NULL)")
        .unwrap();
    db.query("CREATE INDEX ON orders (user_id)").unwrap();
    for user in 0..20 {
        db.query(&format!(
            "INSERT INTO users (id, name) VALUES ('u-{user:03}', 'user-{user:03}')"
        ))
        .unwrap();
        for order in 0..5 {
            let score = user * 10 + order;
            db.query(&format!(
                "INSERT INTO orders (id, user_id, score) VALUES \
                 ('o-{user:03}-{order}', 'u-{user:03}', {score})"
            ))
            .unwrap();
        }
    }

    let result = rows(
        db.query(
            "SELECT u.name, o.score FROM users u \
             JOIN orders o ON o.user_id = u.id \
             ORDER BY o.score DESC LIMIT 5",
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 5);
    assert_eq!(result[0][1], Value::Int64(194));
    assert_eq!(result[4][1], Value::Int64(190));
    assert!(db.query_memory_stats().spill_files > 0);
    assert!(spill_dir.read_dir().unwrap().next().is_none());
}

#[test]
fn unindexed_grace_hash_join_is_bounded_even_for_a_hot_key() {
    let dir = tempfile::tempdir().unwrap();
    let spill_dir = dir.path().join("grace-join-spill");
    let db = Db::create_with(
        dir.path().join("grace.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 384,
                scan_batch_rows: 3,
                spill_directory: Some(spill_dir.clone()),
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE lhs (k int64, label text NOT NULL)")
        .unwrap();
    db.query("CREATE TABLE rhs (k int64, payload text NOT NULL)")
        .unwrap();
    for n in 0..8 {
        db.query(&format!(
            "INSERT INTO lhs (id, k, label) VALUES ('l-{n:02}', 7, 'left-{n:02}')"
        ))
        .unwrap();
    }
    db.query("INSERT INTO lhs (id, k, label) VALUES ('l-null', NULL, 'null-key')")
        .unwrap();
    for n in 0..25 {
        db.query(&format!(
            "INSERT INTO rhs (id, k, payload) VALUES ('r-{n:02}', 7, 'right-{n:02}')"
        ))
        .unwrap();
    }
    db.query("INSERT INTO rhs (id, k, payload) VALUES ('r-orphan', 99, 'orphan')")
        .unwrap();

    let joined = rows(
        db.query("SELECT l.id, r.id FROM lhs l JOIN rhs r ON r.k = l.k")
            .unwrap(),
    );
    assert_eq!(joined.len(), 8 * 25);
    assert!(joined
        .iter()
        .all(|row| row[0] != Value::Text("l-null".into())));

    let left = rows(
        db.query("SELECT l.id, r.id FROM lhs l LEFT JOIN rhs r ON r.k = l.k")
            .unwrap(),
    );
    assert_eq!(left.len(), 8 * 25 + 1);
    assert!(left
        .iter()
        .any(|row| row == &[Value::Text("l-null".into()), Value::Null]));

    let right = rows(
        db.query("SELECT l.id, r.id FROM lhs l RIGHT JOIN rhs r ON r.k = l.k")
            .unwrap(),
    );
    assert_eq!(right.len(), 8 * 25 + 1);
    assert!(right
        .iter()
        .any(|row| row == &[Value::Null, Value::Text("r-orphan".into())]));

    let stats = db.query_memory_stats();
    assert!(stats.spill_files > 0);
    assert!(stats.spilled_bytes > 0);
    assert!(spill_dir.read_dir().unwrap().next().is_none());
}

#[test]
fn zero_memory_settings_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let result = Db::create_with(
        dir.path().join("bad.esql"),
        DbOptions {
            memory: MemoryOptions {
                query_working_bytes: 0,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    );
    assert!(matches!(result, Err(Error::InvalidArgument(_))));
}

#[test]
fn database_query_pool_applies_backpressure_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::create_with(
            dir.path().join("global-query-budget.esql"),
            DbOptions {
                memory: MemoryOptions {
                    total_memory_bytes: 16 * 1024,
                    query_pool_bytes: 1024,
                    query_working_bytes: 1024,
                    index_delta_pool_bytes: 4 * 1024,
                    maintenance_pool_bytes: 8 * 1024,
                    reserved_memory_bytes: 3 * 1024,
                    scan_batch_rows: 2,
                    spill_directory: None,
                },
                ..DbOptions::default()
            },
        )
        .unwrap(),
    );
    db.query("CREATE TABLE items (n int64 NOT NULL)").unwrap();
    db.query("INSERT INTO items (id, n) VALUES ('a', 1), ('b', 2)")
        .unwrap();

    let cursor = db.query_cursor("SELECT id, n FROM items").unwrap();
    assert_eq!(db.global_memory_stats().query_in_use_bytes, 1024);
    assert_eq!(
        db.get("items", "a").unwrap().unwrap()["n"],
        Value::Int64(1),
        "a point lookup returns only caller-owned result memory and must not wait for an operator slot"
    );
    assert_eq!(db.global_memory_stats().query_in_use_bytes, 1024);

    let worker_db = db.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = worker_db.query("SELECT n FROM items WHERE id = 'a'");
        tx.send(result).unwrap();
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(75)).is_err(),
        "the second query must wait while the only query permit is held"
    );
    drop(cursor);
    assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
    worker.join().unwrap();

    let stats = db.global_memory_stats();
    assert_eq!(stats.query_in_use_bytes, 0);
    assert_eq!(stats.query_peak_bytes, 1024);
    assert!(stats.query_waits >= 1);
}

#[test]
fn index_delta_pool_consolidates_to_mmap_bases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("global-index-budget.esql");
    let options = DbOptions {
        memory: MemoryOptions {
            total_memory_bytes: 32 * 1024,
            query_pool_bytes: 4 * 1024,
            query_working_bytes: 2 * 1024,
            index_delta_pool_bytes: 3 * 1024,
            maintenance_pool_bytes: 20 * 1024,
            reserved_memory_bytes: 5 * 1024,
            scan_batch_rows: 3,
            spill_directory: Some(dir.path().join("spill")),
        },
        ..DbOptions::default()
    };
    {
        let db = Db::create_with(&path, options.clone()).unwrap();
        db.query("CREATE TABLE docs (tag text NOT NULL, body text NOT NULL)")
            .unwrap();
        db.query("CREATE INDEX ON docs (tag)").unwrap();
        db.create_text_index("docs", "body").unwrap();
        for n in 0..18 {
            db.query(&format!(
                "INSERT INTO docs (id, tag, body) VALUES ('d-{n:02}', 'hot', 'memory budget token {n}')"
            ))
            .unwrap();
        }
        let stats = db.global_memory_stats();
        assert!(stats.index_consolidations > 0);
        assert!(stats.index_delta_bytes <= stats.index_delta_capacity_bytes);
        assert!(stats.maintenance_peak_bytes > 0);
        assert_eq!(
            db.find_eq("docs", "tag", &Value::Text("hot".into()))
                .unwrap()
                .len(),
            18
        );
        assert_eq!(
            db.search_text("docs", "body", "budget", 20, None)
                .unwrap()
                .len(),
            18
        );
    }

    // Consolidation publishes exact, reopenable bases rather than discarding
    // state merely to make an accounting counter smaller.
    let db = Db::open_with(&path, options).unwrap();
    assert_eq!(
        db.find_eq("docs", "tag", &Value::Text("hot".into()))
            .unwrap()
            .len(),
        18
    );
    assert_eq!(
        db.search_text("docs", "body", "budget", 20, None)
            .unwrap()
            .len(),
        18
    );
}

#[test]
fn invalid_global_pool_layout_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let result = Db::create_with(
        dir.path().join("bad-global.esql"),
        DbOptions {
            memory: MemoryOptions {
                total_memory_bytes: 1024,
                query_pool_bytes: 512,
                query_working_bytes: 256,
                index_delta_pool_bytes: 512,
                maintenance_pool_bytes: 512,
                reserved_memory_bytes: 128,
                scan_batch_rows: 1,
                spill_directory: None,
            },
            ..DbOptions::default()
        },
    );
    assert!(matches!(result, Err(Error::InvalidArgument(_))));
}

#[test]
fn oversized_transaction_is_rejected_before_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("transaction-budget.esql"),
        DbOptions {
            memory: MemoryOptions {
                total_memory_bytes: 16 * 1024,
                query_pool_bytes: 2 * 1024,
                query_working_bytes: 1024,
                index_delta_pool_bytes: 2 * 1024,
                maintenance_pool_bytes: 8 * 1024,
                reserved_memory_bytes: 4 * 1024,
                scan_batch_rows: 2,
                spill_directory: None,
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE payloads (body blob NOT NULL)")
        .unwrap();
    let mut transaction = db.begin();
    let mut record = Record::new();
    record.insert("body".into(), Value::Blob(vec![7; 4096]));
    assert!(matches!(
        transaction.insert("payloads", record),
        Err(Error::MemoryLimit(_))
    ));
    assert!(db.scan("payloads").unwrap().is_empty());
}

#[test]
fn vector_deltas_freeze_into_mmap_runs_under_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vector-delta-budget.esql");
    let options = DbOptions {
        memory: MemoryOptions {
            total_memory_bytes: 32 * 1024,
            query_pool_bytes: 4 * 1024,
            query_working_bytes: 4 * 1024,
            index_delta_pool_bytes: 3 * 1024,
            maintenance_pool_bytes: 20 * 1024,
            reserved_memory_bytes: 5 * 1024,
            scan_batch_rows: 2,
            spill_directory: Some(dir.path().join("spill")),
        },
        ..DbOptions::default()
    };
    let mut ids = Vec::new();
    {
        let db = Db::create_with(&path, options.clone()).unwrap();
        db.query("CREATE TABLE vectors (embedding vector(16))")
            .unwrap();
        db.create_vector_index("vectors", "embedding", VectorIndexOptions::default())
            .unwrap();
        for n in 0..20 {
            let mut vector = vec![0.0; 16];
            vector[n % 16] = 1.0;
            let mut record = Record::new();
            record.insert("embedding".into(), Value::Vector(vector));
            ids.push(db.insert("vectors", record).unwrap());
        }
        let stats = db.global_memory_stats();
        assert!(stats.index_consolidations > 0);
        assert!(stats.index_delta_bytes <= stats.index_delta_capacity_bytes);
        db.delete("vectors", &ids[0]).unwrap();
        let hits = db
            .search_vector(
                "vectors",
                "embedding",
                &[
                    1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
                ],
                10,
                &VectorSearchOptions::default(),
            )
            .unwrap();
        assert!(hits.iter().all(|hit| hit.id != ids[0]));
    }
    let db = Db::open_with(&path, options).unwrap();
    let hits = db
        .search_vector(
            "vectors",
            "embedding",
            &[
                1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ],
            10,
            &VectorSearchOptions::default(),
        )
        .unwrap();
    assert!(hits.iter().all(|hit| hit.id != ids[0]));
}

#[test]
fn index_creation_and_primary_recovery_spill_with_a_tiny_maintenance_pool() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bounded-rebuild.esql");
    let options = DbOptions {
        memory: MemoryOptions {
            total_memory_bytes: 20 * 1024,
            query_pool_bytes: 4 * 1024,
            query_working_bytes: 2 * 1024,
            index_delta_pool_bytes: 4 * 1024,
            maintenance_pool_bytes: 8 * 1024,
            reserved_memory_bytes: 4 * 1024,
            scan_batch_rows: 4,
            spill_directory: Some(dir.path().join("spill")),
        },
        ..DbOptions::default()
    };
    {
        let db = Db::create_with(&path, options.clone()).unwrap();
        db.query("CREATE TABLE docs (tag text NOT NULL, body text NOT NULL)")
            .unwrap();
        for n in 0..300 {
            let mut record = Record::new();
            record.insert("id".into(), Value::Text(format!("d-{n:04}")));
            record.insert("tag".into(), Value::Text(format!("g-{}", n % 7)));
            record.insert(
                "body".into(),
                Value::Text(format!("bounded external index construction token {n}")),
            );
            db.insert("docs", record).unwrap();
        }
        db.create_index("docs", "tag", false).unwrap();
        db.create_text_index("docs", "body").unwrap();
        assert_eq!(
            db.find_eq("docs", "tag", &Value::Text("g-3".into()))
                .unwrap()
                .len(),
            43
        );
        db.checkpoint().unwrap();
    }

    // Force the writable-open recovery path. It streams segment entries into
    // external sorted runs instead of rebuilding a record-count-sized map.
    fs::remove_file(path.join("indexes/primary.pidx")).unwrap();
    let db = Db::open_with(&path, options).unwrap();
    assert_eq!(db.scan_batch("docs", None, 7).unwrap().len(), 7);
    assert_eq!(
        db.search_text("docs", "body", "construction", 5, None)
            .unwrap()
            .len(),
        5
    );
    assert!(db.global_memory_stats().index_delta_bytes <= 4 * 1024);
}
