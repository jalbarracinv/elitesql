use elitesql_core::{Db, DbOptions, Error, MemoryOptions, QueryOutput, Value};

fn count(db: &Db, sql: &str) -> usize {
    match db.query(sql).unwrap() {
        QueryOutput::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql).unwrap() {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn ordered_offset_matches_scan_sort_across_batches_and_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let options = DbOptions {
        memory: MemoryOptions {
            scan_batch_rows: 3,
            query_working_bytes: 8192,
            ..MemoryOptions::default()
        },
        ..DbOptions::default()
    };
    let path = dir.path().join("offset.esql");
    let db = Db::create_with(&path, options.clone()).unwrap();
    for table in ["indexed", "oracle"] {
        db.query(&format!(
            "CREATE TABLE {table} (category text, price int, eligible bool)"
        ))
        .unwrap();
        let values = (0..120)
            .map(|i| {
                let category = if i % 3 == 0 { "games" } else { "books" };
                let price = if i % 17 == 0 {
                    "NULL".into()
                } else {
                    (i % 11).to_string()
                };
                format!("('row-{i:03}', '{category}', {price}, {})", i % 5 == 0)
            })
            .collect::<Vec<_>>()
            .join(",");
        db.query(&format!(
            "INSERT INTO {table}(id,category,price,eligible) VALUES {values}"
        ))
        .unwrap();
    }
    db.query("CREATE INDEX ON indexed(category, price)")
        .unwrap();
    let verify = |db: &Db| {
        for filter in [
            "category = 'books'",
            "category = 'books' AND eligible = true",
            "category = 'books' AND price >= 4",
            "category = 'books' AND category = 'books'",
            "category = NULL",
        ] {
            for offset in [0, 1, 8, 47, 1000] {
                for limit in [0, 1, 7] {
                    let tail = format!("WHERE {filter} ORDER BY price, id COLLATE binary LIMIT {limit} OFFSET {offset}");
                    assert_eq!(
                        rows(db, &format!("SELECT id,price FROM indexed {tail}")),
                        rows(db, &format!("SELECT id,price FROM oracle {tail}")),
                        "{tail}",
                    );
                }
            }
        }
    };
    verify(&db);
    db.checkpoint().unwrap();
    for table in ["indexed", "oracle"] {
        db.query(&format!(
            "UPDATE {table} SET category='books', price=-1 WHERE id='row-003'"
        ))
        .unwrap();
        db.query(&format!("UPDATE {table} SET price=99 WHERE id='row-002'"))
            .unwrap();
        db.query(&format!("DELETE FROM {table} WHERE eligible=true"))
            .unwrap();
    }
    verify(&db);
    db.checkpoint().unwrap();
    db.compact().unwrap();
    verify(&db);
    drop(db);
    let db = Db::open_with(&path, options).unwrap();
    verify(&db);
}

#[test]
fn exact_offset_does_not_decode_discarded_wide_records() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("wide.esql"),
        DbOptions {
            memory: MemoryOptions {
                scan_batch_rows: 2,
                query_working_bytes: 4096,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items(category text, price int, body text)")
        .unwrap();
    db.query("CREATE INDEX ON items(category, price)").unwrap();
    db.query_params(
        "INSERT INTO items(category,price,body) VALUES ('books',1,?),('books',2,'small')",
        &[Value::Text("x".repeat(8192))],
    )
    .unwrap();
    for published in [false, true] {
        if published {
            db.checkpoint().unwrap();
        }
        assert_eq!(
            rows(
                &db,
                "SELECT body FROM items WHERE category='books' ORDER BY price LIMIT 1 OFFSET 1"
            ),
            vec![vec![Value::Text("small".into())]]
        );
    }
}

#[test]
fn composite_indexes_coexist_enforce_unique_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("composite.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE items (category text, price int, note text)")
            .unwrap();
        db.query("CREATE INDEX ON items (category)").unwrap();
        db.query("CREATE UNIQUE INDEX ON items (category, price)")
            .unwrap();
        db.query("CREATE INDEX ON items (category, note)").unwrap();
        db.query("INSERT INTO items(id,category,price,note) VALUES ('a','books',10,'one'),('b','books',11,'two'),('c','games',10,'one')")
            .unwrap();
        assert!(matches!(
            db.query(
                "INSERT INTO items(id,category,price,note) VALUES ('d','books',10,'duplicate')"
            ),
            Err(Error::UniqueViolation { .. })
        ));
        // SQL UNIQUE allows any tuple containing NULL to repeat.
        db.query("INSERT INTO items(id,category,price,note) VALUES ('n1','books',NULL,'n'),('n2','books',NULL,'n')")
            .unwrap();
        db.query("UPDATE items SET price = 12 WHERE id = 'b'")
            .unwrap();
        db.checkpoint().unwrap();
        assert_eq!(
            count(&db, "SELECT id FROM items WHERE category = 'books'"),
            4
        );
        db.drop_index_columns("items", &["category".into(), "note".into()])
            .unwrap();
        assert_eq!(db.table_schema("items").unwrap().indexes.len(), 2);
    }

    let reopened = Db::open(&path).unwrap();
    assert_eq!(
        count(&reopened, "SELECT id FROM items WHERE category = 'books'"),
        4
    );
    drop(reopened);
    assert!(elitesql_core::check(&path).unwrap().warnings.is_empty());
}

#[test]
fn composite_index_rejects_repeated_and_unknown_columns() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("bad.esql")).unwrap();
    db.query("CREATE TABLE items (category text, price int)")
        .unwrap();
    assert!(matches!(
        db.query("CREATE INDEX ON items (category, category)"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        db.query("CREATE INDEX ON items (category, missing)"),
        Err(Error::SchemaViolation(_))
    ));
}

#[test]
fn composite_index_streams_compatible_order_and_explain_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("ordered.esql")).unwrap();
    db.query("CREATE TABLE items (category text, price int, note text)")
        .unwrap();
    db.query("CREATE INDEX ON items (category, price)").unwrap();
    db.query(
        "INSERT INTO items(id,category,price,note) VALUES \
        ('a','books',10,'published'),('b','books',30,'published'),\
        ('c','books',20,'published'),('g','games',1,'other')",
    )
    .unwrap();
    db.checkpoint().unwrap();
    // These changes stay in the mutable overlay while the earlier entries are
    // served by the published run. A deletion must hide its old pair and an
    // update must move the row to its new physical tuple position.
    db.query("UPDATE items SET price = 5 WHERE id = 'b'")
        .unwrap();
    db.query("DELETE FROM items WHERE id = 'c'").unwrap();
    db.query("INSERT INTO items(id,category,price,note) VALUES ('d','books',10,'delta')")
        .unwrap();

    let QueryOutput::Rows { rows, .. } = db
        .query(
            "SELECT id, price FROM items WHERE category = 'books' \
             ORDER BY price ASC, id COLLATE binary ASC LIMIT 2 OFFSET 1",
        )
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("a".into()), Value::Int64(10)],
            vec![Value::Text("d".into()), Value::Int64(10)],
        ]
    );

    let QueryOutput::Rows { rows, .. } = db
        .query(
            "EXPLAIN SELECT id FROM items WHERE category = 'books' \
             ORDER BY price ASC, id COLLATE binary ASC LIMIT 2",
        )
        .unwrap()
    else {
        panic!("expected explain rows");
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("LIMIT 2".into())],
            vec![Value::Text(
                "  INDEX ORDERED items (category, price)".into()
            )],
        ]
    );

    // Unicode collation is intentionally not claimed by the physical index.
    let QueryOutput::Rows { rows, .. } = db
        .query(
            "EXPLAIN SELECT id FROM items WHERE category = 'books' \
             ORDER BY note ASC LIMIT 2",
        )
        .unwrap()
    else {
        panic!("expected explain rows");
    };
    assert!(rows
        .iter()
        .any(|row| row == &vec![Value::Text("  SORT items.note ASC".into())]));
}

#[test]
fn ddl_updates_or_removes_every_component_of_a_compound_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("compound-ddl.esql")).unwrap();
    db.query("CREATE TABLE items (category text, price int, note text)")
        .unwrap();
    db.query("CREATE INDEX ON items (category, price)").unwrap();
    db.query("ALTER TABLE items RENAME COLUMN price TO amount")
        .unwrap();
    let schema = db.table_schema("items").unwrap();
    assert_eq!(schema.indexes[0].columns(), ["category", "amount"]);
    db.query("ALTER TABLE items DROP COLUMN amount").unwrap();
    assert!(db.table_schema("items").unwrap().indexes.is_empty());
}
