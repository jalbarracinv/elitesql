use elitesql_core::{Db, DbOptions, Error, MemoryOptions, QueryOutput, Record, Value};

fn rows(output: QueryOutput) -> Vec<Vec<Value>> {
    match output {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn record(fields: &[(&str, Value)]) -> Record {
    fields
        .iter()
        .map(|(k, v)| ((*k).into(), v.clone()))
        .collect()
}

#[test]
fn backup_and_salvage_preserve_physical_ids_and_identity_sequences() {
    for empty in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let db = Db::create(&source).unwrap();
        db.query("CREATE TABLE users(id int AUTO_INCREMENT PRIMARY KEY, name text)")
            .unwrap();
        db.query("INSERT INTO users(id,name) VALUES(1,'keep'),(100,'delete')")
            .unwrap();
        db.query("DELETE FROM users WHERE id = 100").unwrap();
        if empty {
            db.query("DELETE FROM users").unwrap();
        }
        let expected = db.scan("users").unwrap();
        db.backup(dir.path().join("backup")).unwrap();
        db.checkpoint().unwrap();
        drop(db);
        let report = elitesql_core::salvage(&source, dir.path().join("salvage")).unwrap();
        assert_eq!(report.skipped, 0, "healthy input must never lose a row");
        assert_eq!(report.recovered_records, expected.len() as u64);
        for name in ["backup", "salvage"] {
            let path = dir.path().join(name);
            let copy = Db::open(&path).unwrap();
            assert_eq!(
                copy.scan("users").unwrap(),
                expected,
                "{name}: same physical and SQL ids"
            );
            copy.query("INSERT INTO users(name) VALUES('next')")
                .unwrap();
            drop(copy);
            let copy = Db::open(&path).unwrap();
            assert_eq!(
                rows(
                    copy.query("SELECT id FROM users WHERE name = 'next'")
                        .unwrap()
                ),
                vec![vec![Value::Int64(101)]],
                "{name}: deleted identity must not be reused"
            );
        }
    }
}

#[test]
fn corrupt_interior_wal_is_rejected_without_truncating_acknowledged_commits() {
    for corrupt_length in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE docs(body text)").unwrap();
        db.query("INSERT INTO docs(body) VALUES('first-marker')")
            .unwrap();
        db.query("INSERT INTO docs(body) VALUES('second-marker')")
            .unwrap();
        db.query("INSERT INTO docs(body) VALUES('third-marker')")
            .unwrap();
        drop(db);
        let wal = path.join("wal/000001.wal");
        let mut bytes = std::fs::read(&wal).unwrap();
        let offset = bytes
            .windows(b"second-marker".len())
            .position(|window| window == b"second-marker")
            .unwrap();
        if corrupt_length {
            // Corrupt the framed payload length of commit 2 (table + physical key).
            let table = bytes[..offset]
                .windows(4)
                .rposition(|window| window == b"docs")
                .unwrap();
            let id_length =
                u16::from_le_bytes(bytes[table + 4..table + 6].try_into().unwrap()) as usize;
            let payload_length = table + 6 + id_length;
            bytes[payload_length..payload_length + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        } else {
            bytes[offset] ^= 1;
        }
        std::fs::write(&wal, &bytes).unwrap();
        assert!(!elitesql_core::check(&path).unwrap().is_ok());
        assert!(matches!(Db::open(&path), Err(Error::Corrupt(_))));
        assert_eq!(
            std::fs::read(&wal).unwrap(),
            bytes,
            "normal recovery must preserve evidence and later commits"
        );
    }
}

#[test]
fn missing_final_wal_is_detected_by_check_and_open_even_with_manifest_fallback() {
    for fallback in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE docs(body text)").unwrap();
        db.query("INSERT INTO docs(body) VALUES('checkpointed')")
            .unwrap();
        db.checkpoint().unwrap();
        db.query("INSERT INTO docs(body) VALUES('acknowledged')")
            .unwrap();
        drop(db);
        let last = std::fs::read_dir(path.join("wal"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wal"))
            .max()
            .unwrap();
        assert!(std::fs::read(&last)
            .unwrap()
            .windows(12)
            .any(|window| window == b"acknowledged"));
        std::fs::remove_file(&last).unwrap();
        if fallback {
            std::fs::write(path.join("manifest"), b"corrupt").unwrap();
        }
        assert!(!elitesql_core::check(&path).unwrap().is_ok());
        assert!(matches!(Db::open(&path), Err(Error::Corrupt(_))));
        assert!(
            !last.exists(),
            "missing durable WAL must not be replaced with an empty file"
        );
    }
}

#[test]
fn backup_and_salvage_validate_references_after_all_batches_are_copied() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let db = Db::create(&source).unwrap();
    db.query("CREATE TABLE nodes(next text REFERENCES nodes(id))")
        .unwrap();
    let mut txn = db.begin();
    for i in 0..1050 {
        txn.insert(
            "nodes",
            record(&[
                ("id", Value::Text(format!("r{i:04}"))),
                ("next", Value::Text(format!("r{:04}", (i + 1) % 1050))),
            ]),
        )
        .unwrap();
    }
    txn.commit().unwrap();
    let expected = db.scan("nodes").unwrap();
    db.backup(dir.path().join("backup")).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let report = elitesql_core::salvage(&source, dir.path().join("salvage")).unwrap();
    assert_eq!(report.skipped, 0);
    assert_eq!(report.recovered_records, 1050);
    for name in ["backup", "salvage"] {
        let copy = Db::open(dir.path().join(name)).unwrap();
        assert_eq!(copy.scan("nodes").unwrap(), expected);
        assert!(
            copy.query("DELETE FROM nodes WHERE id = 'r0000'").is_err(),
            "{name}: FK must be restored before publication"
        );
    }
}

#[test]
fn integer_order_filters_and_extrema_preserve_all_64_bits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE numbers(n int)").unwrap();
    let values = [
        i64::MAX,
        i64::MAX - 1,
        9007199254740993,
        9007199254740992,
        -9007199254740992,
        -9007199254740993,
        i64::MIN + 1,
        i64::MIN,
    ];
    for value in values {
        db.insert("numbers", record(&[("n", Value::Int64(value))]))
            .unwrap();
    }
    let expected: Vec<_> = values
        .iter()
        .rev()
        .map(|n| vec![Value::Int64(*n)])
        .collect();
    assert_eq!(
        rows(db.query("SELECT n FROM numbers ORDER BY n").unwrap()),
        expected
    );
    assert_eq!(
        rows(db.query("SELECT MIN(n),MAX(n) FROM numbers").unwrap()),
        vec![vec![Value::Int64(i64::MIN), Value::Int64(i64::MAX)]]
    );
    assert_eq!(
        rows(
            db.query("SELECT n FROM numbers WHERE n > 9007199254740992 ORDER BY n")
                .unwrap()
        ),
        vec![
            vec![Value::Int64(9007199254740993)],
            vec![Value::Int64(i64::MAX - 1)],
            vec![Value::Int64(i64::MAX)]
        ]
    );
    assert_eq!(
        rows(
            db.query("SELECT n FROM numbers WHERE n = 9223372036854775808.0")
                .unwrap()
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn numeric_joins_match_with_hash_and_either_index_orientation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE integers(n int)").unwrap();
    db.query("CREATE TABLE floats(n float64)").unwrap();
    for value in [-1, 0, 1, 9007199254740992, 9007199254740993, i64::MAX] {
        db.insert("integers", record(&[("n", Value::Int64(value))]))
            .unwrap();
    }
    for value in [
        -1.5,
        -1.0,
        0.0,
        1.0,
        9007199254740992.0,
        9223372036854775808.0,
    ] {
        db.insert("floats", record(&[("n", Value::Float64(value))]))
            .unwrap();
    }
    let expected: Vec<_> = [-1, 0, 1, 9007199254740992]
        .iter()
        .map(|n| vec![Value::Int64(*n)])
        .collect();
    for index in [None, Some("floats"), Some("integers")] {
        if let Some(table) = index {
            db.query(&format!("CREATE INDEX ON {table}(n)")).unwrap();
        }
        for sql in [
            "SELECT i.n FROM integers i JOIN floats f ON i.n = f.n ORDER BY i.n",
            "SELECT i.n FROM floats f JOIN integers i ON f.n = i.n ORDER BY i.n",
        ] {
            assert_eq!(rows(db.query(sql).unwrap()), expected, "{index:?}: {sql}");
            let unordered = sql.replace(" ORDER BY i.n", "");
            let mut actual = rows(db.query(&unordered).unwrap());
            actual.sort_by_key(|row| match row[0] {
                Value::Int64(n) => n,
                _ => panic!("expected integer"),
            });
            assert_eq!(actual, expected, "unordered {index:?}: {sql}");
        }
        if let Some(table) = index {
            db.query(&format!("DROP INDEX ON {table}(n)")).unwrap();
        }
    }
}

fn rewrite_manifest(path: &std::path::Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let manifest = path.join("manifest");
    let bytes = std::fs::read(&manifest).unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&bytes[16..]).unwrap();
    edit(&mut json);
    let body = serde_json::to_vec(&json).unwrap();
    let mut updated = b"ESQLMANI".to_vec();
    updated.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    updated.extend_from_slice(&(body.len() as u32).to_le_bytes());
    updated.extend(body);
    std::fs::write(manifest, updated).unwrap();
}

#[test]
fn deep_check_rejects_checksum_valid_orphans_duplicates_and_bad_sequences() {
    for problem in ["orphan", "duplicate", "identity"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Db::create(&path).unwrap();
        match problem {
            "orphan" => {
                db.query("CREATE TABLE parents(code int)").unwrap();
                db.query("CREATE UNIQUE INDEX ON parents(code)").unwrap();
                db.query("CREATE TABLE children(code int)").unwrap();
                db.query("CREATE INDEX ON children(code)").unwrap();
                db.query("INSERT INTO children(code) VALUES(2)").unwrap();
            }
            "duplicate" => {
                db.query("CREATE TABLE items(code int)").unwrap();
                db.query("CREATE INDEX ON items(code)").unwrap();
                db.query("INSERT INTO items(code) VALUES(1),(1)").unwrap();
            }
            _ => {
                db.query("CREATE TABLE items(id int AUTO_INCREMENT PRIMARY KEY)")
                    .unwrap();
                db.query("INSERT INTO items(id) VALUES(100)").unwrap();
            }
        }
        db.checkpoint().unwrap();
        drop(db);
        rewrite_manifest(&path, |manifest| match problem {
            "orphan" => {
                manifest["catalog"]["tables"][1]["foreign_keys"] = serde_json::json!([{"column":"code","referenced_table":"parents","referenced_column":"code","on_delete":"restrict"}])
            }
            "duplicate" => {
                manifest["catalog"]["tables"][0]["indexes"][0]["unique"] = serde_json::json!(true)
            }
            _ => manifest["identity_high_water"]["items"] = serde_json::json!(1),
        });
        let before = std::fs::read(path.join("manifest")).unwrap();
        let report = elitesql_core::check(&path).unwrap();
        assert!(
            !report.is_ok(),
            "{problem}: checksum-valid logical damage must be rejected"
        );
        assert!(
            report.errors.iter().any(|error| error.contains(problem)),
            "{problem}: {:?}",
            report.errors
        );
        assert_eq!(
            std::fs::read(path.join("manifest")).unwrap(),
            before,
            "check is read-only"
        );
    }
}

#[test]
fn offline_check_refuses_an_active_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    assert!(matches!(
        elitesql_core::check(&path),
        Err(Error::DatabaseLocked(_))
    ));
    drop(db);
    assert!(elitesql_core::check(&path).unwrap().is_ok());
}

#[test]
fn indexed_cursor_keeps_snapshot_when_a_later_commit_changes_membership() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("db"),
        DbOptions {
            memory: MemoryOptions {
                scan_batch_rows: 1,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items(n int, label text)").unwrap();
    db.query("CREATE INDEX ON items(n)").unwrap();
    db.query("INSERT INTO items(id,n,label) VALUES('a',1,'yes'),('b',2,'yes'),('c',1,'yes')")
        .unwrap();
    for sql in [
        "EXPLAIN SELECT id FROM items WHERE label = 'yes' AND n = 1",
        "EXPLAIN SELECT id FROM items WHERE n = 1 AND label = 'yes'",
    ] {
        let plan = format!("{:?}", db.query(sql).unwrap());
        assert!(plan.to_lowercase().contains("index"), "{plan}");
    }
    let mut cursor = db
        .query_cursor("SELECT id FROM items WHERE label = 'yes' AND n = 1")
        .unwrap();
    assert_eq!(
        cursor.next().unwrap().unwrap(),
        vec![Value::Text("a".into())]
    );
    db.query("UPDATE items SET n = 2 WHERE id = 'c'").unwrap();
    db.query("UPDATE items SET n = 1 WHERE id = 'b'").unwrap();
    assert_eq!(
        cursor.next().unwrap().unwrap(),
        vec![Value::Text("c".into())]
    );
    assert!(cursor.next().is_none());
    let cursors = (0..20)
        .map(|_| db.query_cursor("SELECT id FROM items").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(db.global_memory_stats().query_in_use_bytes, 0);
    assert!(db.query("SELECT id FROM items WHERE id = 'a'").is_ok());
    drop(cursors);
}

#[test]
fn cancellation_and_deadlines_release_query_state_without_leaking_to_later_queries() {
    use elitesql_core::QueryControl;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE items(n int)").unwrap();
    db.query("INSERT INTO items(n) VALUES(1),(2),(3)").unwrap();
    let mut cursor = db.query_cursor("SELECT n FROM items").unwrap();
    cursor.next().unwrap().unwrap();
    let cancel = cursor.control();
    std::thread::spawn(move || cancel.cancel()).join().unwrap();
    assert!(matches!(
        cursor.next(),
        Some(Err(Error::QueryInterrupted(_)))
    ));
    assert!(cursor.next().is_none());
    assert_eq!(db.global_memory_stats().query_in_use_bytes, 0);
    let deadline = QueryControl::with_timeout(Duration::ZERO).unwrap();
    assert!(matches!(
        deadline.run(|| db.query("SELECT n FROM items")),
        Err(Error::QueryInterrupted(_))
    ));
    assert_eq!(rows(db.query("SELECT n FROM items").unwrap()).len(), 3);
    // Once a caller starts commit, cancellation cannot cause a partial or
    // falsely rejected durable publication.
    let control = QueryControl::default();
    control
        .run(|| {
            let mut txn = db.begin();
            txn.insert("items", record(&[("n", Value::Int64(4))]))?;
            control.cancel();
            txn.commit()
        })
        .unwrap();
    assert_eq!(rows(db.query("SELECT n FROM items").unwrap()).len(), 4);
}

#[test]
fn byte_bounded_scans_return_wide_rows_without_advancing_past_a_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE items(body text)").unwrap();
    for id in ["a", "b", "c"] {
        db.insert(
            "items",
            record(&[
                ("id", Value::Text(id.into())),
                ("body", Value::Text("x".repeat(3000))),
            ]),
        )
        .unwrap();
    }
    let snapshot = db.snapshot();
    let first = db
        .scan_batch_at_bytes(&snapshot, "items", None, 512, 4096)
        .unwrap();
    assert_eq!(first.len(), 1);
    let second = db
        .scan_batch_at_bytes(&snapshot, "items", Some(&first[0].0), 512, 4096)
        .unwrap();
    assert_eq!(second[0].0, "b");
    assert!(matches!(
        db.scan_batch_at_bytes(&snapshot, "items", None, 512, 512),
        Err(Error::MemoryLimit(_))
    ));
    assert_eq!(
        db.scan_batch_at_bytes(&snapshot, "items", None, 512, 4096)
            .unwrap()[0]
            .0,
        "a"
    );
}

#[test]
fn staging_accounts_for_nested_json_heap_before_accepting_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("db"),
        DbOptions {
            memory: MemoryOptions {
                index_delta_pool_bytes: 4096,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items(payload json)").unwrap();
    let mut txn = db.begin();
    assert!(matches!(
        txn.insert(
            "items",
            record(&[(
                "payload",
                Value::Json(serde_json::json!({"nested":["x".repeat(8192)]}))
            )])
        ),
        Err(Error::MemoryLimit(_))
    ));
    txn.commit().unwrap();
    assert!(db.scan("items").unwrap().is_empty());
}

#[test]
fn replacing_a_referenced_parent_cannot_leave_orphans() {
    for action in ["RESTRICT", "CASCADE"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE parents (code int)").unwrap();
        db.query("CREATE UNIQUE INDEX ON parents(code)").unwrap();
        db.query(&format!(
            "CREATE TABLE children (parent_code int REFERENCES parents(code) ON DELETE {action})"
        ))
        .unwrap();
        db.query("INSERT INTO parents(id,code) VALUES('p',1)")
            .unwrap();
        db.query("INSERT INTO children(parent_code) VALUES(1)")
            .unwrap();
        let mut tx = db.begin();
        tx.delete("parents", "p").unwrap();
        tx.insert(
            "parents",
            record(&[("id", Value::Text("p".into())), ("code", Value::Int64(2))]),
        )
        .unwrap();
        assert!(
            matches!(tx.commit(), Err(Error::SchemaViolation(_))),
            "replacement must reject a referenced key change ({action})"
        );
        db.checkpoint().unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            rows(db.query("SELECT code FROM parents").unwrap()),
            vec![vec![Value::Int64(1)]]
        );
        assert_eq!(
            rows(db.query("SELECT parent_code FROM children").unwrap()),
            vec![vec![Value::Int64(1)]]
        );
    }
}

#[test]
fn parent_update_revalidates_foreign_keys_created_after_staging() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE parents(code int)").unwrap();
    db.query("CREATE UNIQUE INDEX ON parents(code)").unwrap();
    db.query("INSERT INTO parents(id,code) VALUES('p',1)")
        .unwrap();
    let mut tx = db.begin();
    tx.update("parents", "p", record(&[("code", Value::Int64(2))]))
        .unwrap();
    db.query("CREATE TABLE children(parent_code int REFERENCES parents(code))")
        .unwrap();
    db.query("INSERT INTO children(parent_code) VALUES(1)")
        .unwrap();
    assert!(matches!(tx.commit(), Err(Error::SchemaViolation(_))));
    assert_eq!(
        rows(db.query("SELECT code FROM parents").unwrap()),
        vec![vec![Value::Int64(1)]]
    );
}

#[test]
fn identity_primary_key_cannot_lose_its_supporting_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE users(id int AUTO_INCREMENT PRIMARY KEY, name text)")
        .unwrap();
    db.query("INSERT INTO users(id,name) VALUES(1,'a')")
        .unwrap();
    assert!(matches!(
        db.query("DROP INDEX ON users(id)"),
        Err(Error::SchemaViolation(_))
    ));
    assert!(matches!(
        db.query("INSERT INTO users(id,name) VALUES(1,'b')"),
        Err(Error::UniqueViolation { .. })
    ));
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert!(matches!(
        db.query("INSERT INTO users(id,name) VALUES(1,'b')"),
        Err(Error::UniqueViolation { .. })
    ));
}

#[test]
fn failed_statement_restores_staging_and_preserves_earlier_statements() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE nums(n int NOT NULL)").unwrap();
    db.query("INSERT INTO nums(id,n) VALUES('a',1),('b',9223372036854775807)")
        .unwrap();
    let mut tx = db.begin();
    tx.query("UPDATE nums SET n=10 WHERE id='a'").unwrap();
    assert!(tx.query("UPDATE nums SET n=n+1").is_err());
    assert_eq!(tx.get("nums", "a").unwrap().unwrap()["n"], Value::Int64(10));
    assert!(tx
        .query("INSERT INTO nums(id,n) VALUES('c',3),('d',NULL)")
        .is_err());
    assert!(tx.get("nums", "c").unwrap().is_none());
    tx.query("INSERT INTO nums(id,n) VALUES('e',5)").unwrap();
    tx.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(
        rows(db.query("SELECT n FROM nums ORDER BY id").unwrap()),
        vec![
            vec![Value::Int64(10)],
            vec![Value::Int64(i64::MAX)],
            vec![Value::Int64(5)]
        ]
    );
}

#[test]
fn failed_first_statement_removes_new_table_staging() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE nums(n int NOT NULL)").unwrap();
    let mut tx = db.begin();
    assert!(tx
        .query("INSERT INTO nums(id,n) VALUES('a',1),('b',NULL)")
        .is_err());
    tx.query("INSERT INTO nums(id,n) VALUES('a',2)").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        rows(db.query("SELECT n FROM nums").unwrap()),
        vec![vec![Value::Int64(2)]]
    );
}

#[test]
fn ignored_duplicates_do_not_make_multirow_insert_partially_atomic() {
    for ignore in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE items(n int NOT NULL)").unwrap();
        db.query("CREATE UNIQUE INDEX ON items(n)").unwrap();
        db.query("INSERT INTO items(id,n) VALUES('old',1)").unwrap();
        let sql = |values: &str| {
            if ignore {
                format!("INSERT IGNORE INTO items(id,n) VALUES {values}")
            } else {
                format!("INSERT INTO items(id,n) VALUES {values} ON CONFLICT DO NOTHING")
            }
        };
        assert!(db
            .query(&sql("('duplicate',1),('a',2),('b',NULL)"))
            .is_err());
        assert_eq!(
            rows(db.query("SELECT n FROM items").unwrap()),
            vec![vec![Value::Int64(1)]]
        );
        let before = db.snapshot().version();
        db.query(&sql("('duplicate',1),('a',2),('b',2),('c',3)"))
            .unwrap();
        assert_eq!(
            db.snapshot().version(),
            before + 1,
            "one WAL commit per SQL statement"
        );
        db.checkpoint().unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            rows(db.query("SELECT n FROM items ORDER BY n").unwrap()),
            vec![
                vec![Value::Int64(1)],
                vec![Value::Int64(2)],
                vec![Value::Int64(3)]
            ]
        );
    }
}

#[test]
fn corrupt_primary_navigation_is_rebuilt_before_accepting_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE items(n int)").unwrap();
    db.query("INSERT INTO items(id,n) VALUES('r1',1),('r2',2),('r3',3)")
        .unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let index_path = path.join("indexes/primary.pidx");
    let mut bytes = std::fs::read(&index_path).unwrap();
    let directory = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let page =
        u64::from_le_bytes(bytes[directory + 4..directory + 12].try_into().unwrap()) as usize;
    let first_len = u32::from_le_bytes(bytes[page + 8..page + 12].try_into().unwrap()) as usize;
    assert_eq!(bytes[page + 16 + first_len - 1], b'1');
    bytes[page + 16 + first_len - 1] ^= 2;
    std::fs::write(index_path, bytes).unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(
        rows(db.query("SELECT n FROM items WHERE id='r1'").unwrap()),
        vec![vec![Value::Int64(1)]]
    );
    assert!(matches!(
        db.query("INSERT INTO items(id,n) VALUES('r1',99)"),
        Err(Error::DuplicateId { .. })
    ));
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(
        rows(db.query("SELECT n FROM items ORDER BY id").unwrap()),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)]
        ]
    );
}

#[test]
fn failed_statement_preserves_pending_cascade_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE parents(code int NOT NULL)").unwrap();
    db.query("CREATE UNIQUE INDEX ON parents(code)").unwrap();
    db.query("CREATE TABLE children(parent_code int REFERENCES parents(code) ON DELETE CASCADE)")
        .unwrap();
    db.query("INSERT INTO parents(id,code) VALUES('p',1)")
        .unwrap();
    db.query("INSERT INTO children(parent_code) VALUES(1)")
        .unwrap();
    let mut tx = db.begin();
    tx.delete("parents", "p").unwrap();
    assert!(tx
        .query("INSERT INTO parents(id,code) VALUES('p',2),('bad',NULL)")
        .is_err());
    assert!(tx.get("parents", "p").unwrap().is_none());
    tx.commit().unwrap();
    assert!(db.scan("parents").unwrap().is_empty());
    assert!(db.scan("children").unwrap().is_empty());
}

#[test]
fn statement_memory_failure_restores_prior_staged_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("db"),
        DbOptions {
            memory: MemoryOptions {
                index_delta_pool_bytes: 4096,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items(body text)").unwrap();
    let mut tx = db.begin();
    tx.query("INSERT INTO items(id,body) VALUES('old','keep')")
        .unwrap();
    let payload = Value::Text("x".repeat(2000));
    assert!(matches!(
        tx.query_params(
            "INSERT INTO items(id,body) VALUES('a',?),('b',?)",
            &[payload.clone(), payload]
        ),
        Err(Error::MemoryLimit(_))
    ));
    assert!(tx.get("items", "a").unwrap().is_none());
    tx.query("INSERT INTO items(id,body) VALUES('next','ok')")
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(db.scan("items").unwrap().len(), 2);
}

#[test]
fn concurrent_ignore_retries_the_whole_statement() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(dir.path().join("db")).unwrap());
    db.query("CREATE TABLE items(n int)").unwrap();
    db.query("CREATE UNIQUE INDEX ON items(n)").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (1..=2)
        .map(|n| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.query(&format!("INSERT IGNORE INTO items(n) VALUES({n}),(99)"))
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap().unwrap();
    }
    assert_eq!(
        rows(db.query("SELECT n FROM items ORDER BY n").unwrap()),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(99)]
        ]
    );
    assert_eq!(db.snapshot().version(), 2);
}

#[test]
fn adding_an_identity_to_an_empty_table_preserves_uniqueness() {
    use elitesql_core::{Column, ColumnType};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE items(body text)").unwrap();
    db.add_column("items", Column::new("serial", ColumnType::Int64).identity())
        .unwrap();
    db.query("INSERT INTO items(serial,body) VALUES(1,'a')")
        .unwrap();
    assert!(matches!(
        db.query("INSERT INTO items(serial,body) VALUES(1,'b')"),
        Err(Error::UniqueViolation { .. })
    ));
    assert!(db
        .add_column(
            "items",
            Column::new("another", ColumnType::Int64).identity()
        )
        .is_err());
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(
        rows(db.query("SELECT serial FROM items").unwrap()),
        vec![vec![Value::Int64(1)]]
    );
}

// --- 2026-09-12 review (R01–R18) ---------------------------------------------

fn write_intent(path: &std::path::Path, json: &str) {
    std::fs::write(path.join("ddl.json"), json).unwrap();
}

#[test]
fn recovery_of_a_pending_add_column_keeps_every_derived_index_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE users(email text NOT NULL, bio text)")
        .unwrap();
    db.query("CREATE UNIQUE INDEX ON users(email)").unwrap();
    db.create_text_index("users", "bio").unwrap();
    db.query("INSERT INTO users(email,bio) VALUES('a@x','loves databases')")
        .unwrap();
    assert!(matches!(
        db.query("INSERT INTO users(email) VALUES('a@x')"),
        Err(Error::UniqueViolation { .. })
    ));
    db.checkpoint().unwrap();
    drop(db);

    // A crash between the intent and its completion.
    write_intent(
        &path,
        r#"{"op":"AddColumn","table":"users","column":{"name":"plan","type":"text","nullable":true},"not_null":false}"#,
    );
    let db = Db::open(&path).unwrap();
    assert!(!path.join("ddl.json").exists());
    assert!(
        matches!(
            db.query("INSERT INTO users(email) VALUES('a@x')"),
            Err(Error::UniqueViolation { .. })
        ),
        "the unique index must be enforced by the recovering process itself"
    );
    assert!(
        db.query("INSERT IGNORE INTO users(email) VALUES('a@x')")
            .is_ok(),
        "INSERT IGNORE still sees the index"
    );
    assert_eq!(
        db.search_text("users", "bio", "databases", 10, None)
            .unwrap()
            .len(),
        1,
        "the text index is loaded, not silently empty"
    );
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(rows(db.query("SELECT email FROM users").unwrap()).len(), 1);
}

fn last_wal(path: &std::path::Path) -> std::path::PathBuf {
    let mut files: Vec<_> = std::fs::read_dir(path.join("wal"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| file.extension().is_some_and(|ext| ext == "wal"))
        .collect();
    files.sort();
    files.pop().unwrap()
}

#[test]
fn zero_filled_and_garbage_wal_tails_are_torn_tails_not_corruption() {
    // A power loss can extend the WAL with zeros or a partially written
    // block instead of the exact byte prefix a process kill leaves.
    fn zeros(bytes: &mut Vec<u8>, cut: usize) {
        bytes.truncate(cut);
        bytes.extend(std::iter::repeat_n(0u8, 4096));
    }
    fn garbage(bytes: &mut Vec<u8>, cut: usize) {
        bytes.truncate(cut);
        bytes.extend((0..300u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8));
    }
    fn bit_rot(bytes: &mut [u8], cut: usize) {
        let last = bytes.len() - 1;
        bytes[cut.max(last - 3)] ^= 0x55;
    }
    type Tail = fn(&mut Vec<u8>, usize);
    let cases: [(&str, Tail); 3] = [
        ("zeros", zeros),
        ("garbage", garbage),
        ("bit rot inside the final record", |bytes, cut| {
            bit_rot(bytes, cut)
        }),
    ];
    for (label, tail) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE docs(body text)").unwrap();
        db.query("INSERT INTO docs(body) VALUES('first-marker')")
            .unwrap();
        db.query("INSERT INTO docs(body) VALUES('second-marker')")
            .unwrap();
        drop(db);
        let wal = last_wal(&path);
        let mut bytes = std::fs::read(&wal).unwrap();
        let second = bytes
            .windows(b"second-marker".len())
            .position(|window| window == b"second-marker")
            .unwrap();
        // Cut inside the second record's payload; its header stays intact.
        tail(&mut bytes, second + 3);
        std::fs::write(&wal, &bytes).unwrap();

        let report = elitesql_core::check(&path).unwrap();
        assert!(report.is_ok(), "{label}: check errors {:?}", report.errors);
        let db = Db::open(&path).unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(
            rows(db.query("SELECT body FROM docs").unwrap()),
            vec![vec![Value::Text("first-marker".into())]],
            "{label}: the incomplete commit disappears, the earlier one stays"
        );
        db.query("INSERT INTO docs(body) VALUES('after')").unwrap();
        drop(db);
        assert_eq!(
            rows(
                Db::open(&path)
                    .unwrap()
                    .query("SELECT body FROM docs")
                    .unwrap()
            )
            .len(),
            2
        );
    }
}

#[test]
fn torn_wal_before_empty_reserved_successors_is_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE docs(body text)").unwrap();
    db.query("INSERT INTO docs(body) VALUES('kept')").unwrap();
    db.query("INSERT INTO docs(body) VALUES('torn-marker')")
        .unwrap();
    drop(db);
    let wal = last_wal(&path);
    let mut bytes = std::fs::read(&wal).unwrap();
    let cut = bytes
        .windows(b"torn-marker".len())
        .position(|window| window == b"torn-marker")
        .unwrap();
    bytes.truncate(cut);
    std::fs::write(&wal, &bytes).unwrap();
    // The empty successors a checkpoint reserves before syncing the old WAL.
    std::fs::write(path.join("wal/000002.wal"), b"").unwrap();
    std::fs::write(path.join("wal/000003.wal"), b"").unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(rows(db.query("SELECT body FROM docs").unwrap()).len(), 1);
}

#[test]
fn recovery_records_the_wal_extent_it_resumes_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE docs(body text)").unwrap();
    db.query("INSERT INTO docs(body) VALUES('one')").unwrap();
    drop(db);
    // Successors created by an interrupted checkpoint, never recorded.
    std::fs::write(path.join("wal/000002.wal"), b"").unwrap();
    std::fs::write(path.join("wal/000003.wal"), b"").unwrap();
    let db = Db::open(&path).unwrap();
    db.query("INSERT INTO docs(body) VALUES('two')").unwrap();
    drop(db);
    // The writer resumed in 000003.wal; losing it must be detectable now.
    std::fs::remove_file(path.join("wal/000003.wal")).unwrap();
    assert!(
        matches!(Db::open(&path), Err(Error::Corrupt(_))),
        "a required successor cannot vanish into an older-looking chain"
    );
    assert!(!elitesql_core::check(&path).unwrap().is_ok());
}

#[test]
fn transactional_update_works_with_a_declared_integer_primary_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE users(id int AUTO_INCREMENT PRIMARY KEY, n int)")
        .unwrap();
    db.query("INSERT INTO users(n) VALUES(1),(2)").unwrap();
    let mut tx = db.begin();
    assert_eq!(
        tx.query("UPDATE users SET n = n + 10").unwrap(),
        QueryOutput::Affected(2)
    );
    tx.commit().unwrap();
    assert_eq!(
        rows(db.query("SELECT n FROM users ORDER BY id").unwrap()),
        vec![vec![Value::Int64(11)], vec![Value::Int64(12)]]
    );
}

#[test]
fn table_names_with_control_characters_are_rejected() {
    use elitesql_core::{Column, ColumnType, TableSchema};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    for name in ["\0elitesql_identity", "a\nb", "x\u{7f}"] {
        assert!(matches!(
            db.create_table(TableSchema::new(
                name,
                vec![Column::new("n", ColumnType::Int64)]
            )),
            Err(Error::InvalidArgument(_))
        ));
    }
    drop(db);
    Db::open(&path).unwrap();
}

#[test]
fn float_zero_signs_and_nan_agree_across_indexes_equality_and_storage() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE m(x float64)").unwrap();
    db.query("CREATE UNIQUE INDEX ON m(x)").unwrap();
    db.query("INSERT INTO m(x) VALUES(0.0)").unwrap();
    assert!(
        matches!(
            db.query("INSERT INTO m(x) VALUES(-0.0)"),
            Err(Error::UniqueViolation { .. })
        ),
        "-0.0 equals 0.0 for the unique index"
    );
    assert_eq!(
        rows(db.query("SELECT x FROM m WHERE x = -0.0").unwrap()).len(),
        1
    );
    let stored = db.scan("m").unwrap();
    assert!(matches!(stored[0].1["x"], Value::Float64(x) if x.is_sign_positive()));
    let mut nan = record(&[("x", Value::Float64(f64::NAN))]);
    assert!(matches!(
        db.insert("m", nan.clone()),
        Err(Error::SchemaViolation(_))
    ));
    nan.insert("x".into(), Value::Float64(-0.0));
    let id = db
        .insert("m", record(&[("x", Value::Float64(1.5))]))
        .unwrap();
    let mut tx = db.begin();
    tx.update("m", &id, record(&[("x", Value::Float64(-0.0))]))
        .unwrap();
    assert!(matches!(tx.commit(), Err(Error::UniqueViolation { .. })));
}

#[test]
fn identity_column_cannot_be_dropped_and_json_parameters_match_literals() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("db")).unwrap();
    db.query("CREATE TABLE users(id int AUTO_INCREMENT PRIMARY KEY, doc json, d date)")
        .unwrap();
    assert!(matches!(
        db.query("ALTER TABLE users DROP COLUMN id"),
        Err(Error::SchemaViolation(_))
    ));
    db.query("INSERT INTO users(doc) VALUES('{\"a\":1}')")
        .unwrap();
    db.query_params(
        "INSERT INTO users(doc) VALUES(?)",
        &[Value::Text("{\"a\":1}".into())],
    )
    .unwrap();
    let docs = rows(db.query("SELECT doc FROM users ORDER BY id").unwrap());
    assert_eq!(
        docs[0], docs[1],
        "literal and bound text store the same JSON shape"
    );
    assert!(
        db.query_params(
            "INSERT INTO users(doc) VALUES(?)",
            &[Value::Text("not json".into())],
        )
        .is_err(),
        "text that does not parse as JSON is rejected like the literal"
    );
    assert!(db.query("INSERT INTO users(d) VALUES(100000000)").is_err());
    assert!(db
        .query_params(
            "INSERT INTO users(d) VALUES(?)",
            &[Value::Int64(100_000_000)]
        )
        .is_err());
}

#[test]
fn read_only_open_reports_what_it_could_not_expose() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::create(&path).unwrap();
    db.query("CREATE TABLE docs(body text)").unwrap();
    for i in 0..20 {
        db.query(&format!("INSERT INTO docs(body) VALUES('row {i}')"))
            .unwrap();
    }
    db.checkpoint().unwrap();
    drop(db);
    assert!(Db::open_read_only(&path)
        .unwrap()
        .recovery_warnings()
        .is_empty());
    let segment = path.join("segments/000001.seg");
    let mut bytes = std::fs::read(&segment).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&segment, &bytes).unwrap();
    assert!(matches!(Db::open(&path), Err(Error::Corrupt(_))));
    let db = Db::open_read_only(&path).unwrap();
    let warnings = db.recovery_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("000001.seg")),
        "a partial view must be announced: {warnings:?}"
    );
    assert!(db.scan("docs").unwrap().len() < 20);
}
