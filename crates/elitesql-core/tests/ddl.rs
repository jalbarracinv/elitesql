//! DROP and ALTER: effect, durability, space reclamation and crash recovery.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};

use elitesql_core::{
    check, Column, ColumnType, Db, Error, QueryOutput, Record, TableSchema, Value,
    VectorIndexOptions, VectorSearchOptions,
};

fn new_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("app.esql")).unwrap();
    (dir, db)
}

fn path_of(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("app.esql")
}

fn users() -> TableSchema {
    TableSchema::new(
        "users",
        vec![
            Column::new("name", ColumnType::Text).not_null(),
            Column::new("age", ColumnType::Int64),
        ],
    )
}

fn insert(db: &Db, table: &str, pairs: &[(&str, Value)]) -> String {
    let mut r = Record::new();
    for (k, v) in pairs {
        r.insert((*k).to_owned(), v.clone());
    }
    db.insert(table, r).unwrap()
}

fn rows(out: QueryOutput) -> Vec<Vec<Value>> {
    match out {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn headers(out: QueryOutput) -> Vec<String> {
    match out {
        QueryOutput::Rows { columns, .. } => columns,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn assert_clean(path: &Path) {
    let report = check(path).unwrap();
    assert!(report.is_ok(), "check failed: {:?}", report.errors);
}

#[test]
fn concurrent_catalog_changes_are_serialized_from_validation_to_publish() {
    let (dir, db) = new_db();
    db.create_table(users()).unwrap();
    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            db.add_column("users", Column::new("nickname", ColumnType::Text))
        }));
    }
    barrier.wait();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        db.table_schema("users")
            .unwrap()
            .columns
            .iter()
            .filter(|column| column.name == "nickname")
            .count(),
        1
    );
    assert_clean(&path_of(&dir));
}

// --- DROP TABLE ---------------------------------------------------------------

#[test]
fn drop_table_is_durable_and_reclaims_space_on_compact() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    db.create_table(TableSchema::new(
        "keep",
        vec![Column::new("v", ColumnType::Text)],
    ))
    .unwrap();
    for i in 0..200 {
        insert(
            &db,
            "users",
            &[
                ("name", Value::Text(format!("u{i}"))),
                ("age", Value::Int64(i)),
            ],
        );
    }
    insert(&db, "keep", &[("v", Value::Text("survivor".into()))]);
    db.checkpoint().unwrap();
    let before = segment_bytes(&path);

    db.drop_table("users").unwrap();
    assert!(matches!(db.tables().as_slice(), [t] if t == "keep"));
    assert!(matches!(db.scan("users"), Err(Error::TableNotFound(_))));
    assert!(matches!(
        db.query("SELECT * FROM users"),
        Err(Error::TableNotFound(_))
    ));
    assert!(matches!(
        db.drop_table("users"),
        Err(Error::TableNotFound(_))
    ));
    // The other table is untouched.
    assert_eq!(db.scan("keep").unwrap().len(), 1);
    drop(db);

    // Still gone after reopening, and the records are unreachable even though
    // their bytes are still in the old segment.
    let db = Db::open(&path).unwrap();
    assert_eq!(db.tables(), vec!["keep".to_string()]);
    assert!(matches!(db.scan("users"), Err(Error::TableNotFound(_))));
    assert_clean(&path);

    // Compaction is what returns the disk space.
    db.compact().unwrap();
    let after = segment_bytes(&path);
    assert!(
        after * 4 < before,
        "compaction should reclaim the dropped table's space (before {before}, after {after})"
    );
    assert_eq!(db.scan("keep").unwrap().len(), 1);
    drop(db);
    assert_clean(&path);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("keep").unwrap().len(), 1);
}

fn segment_bytes(db_path: &Path) -> u64 {
    fs::read_dir(db_path.join("segments"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "seg"))
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

#[test]
fn recreating_a_dropped_table_does_not_resurrect_its_records() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    for i in 0..5 {
        insert(
            &db,
            "users",
            &[
                ("name", Value::Text(format!("old{i}"))),
                ("age", Value::Int64(i)),
            ],
        );
    }
    db.checkpoint().unwrap();
    db.drop_table("users").unwrap();
    db.create_table(users()).unwrap();
    insert(&db, "users", &[("name", Value::Text("fresh".into()))]);

    let expect_fresh = |db: &Db| {
        let all = db.scan("users").unwrap();
        assert_eq!(all.len(), 1, "old records came back: {all:?}");
        assert_eq!(all[0].1["name"], Value::Text("fresh".into()));
    };
    expect_fresh(&db);
    drop(db);

    // The old rows are still in the segment written before the drop: the
    // table's epoch is what keeps them out.
    let db = Db::open(&path).unwrap();
    expect_fresh(&db);
    db.compact().unwrap();
    expect_fresh(&db);
    drop(db);
    assert_clean(&path);
    let db = Db::open(&path).unwrap();
    expect_fresh(&db);
    // A unique index built after the drop must not see the old values either.
    db.create_index("users", "name", true).unwrap();
    insert(&db, "users", &[("name", Value::Text("old0".into()))]);
    assert_eq!(db.scan("users").unwrap().len(), 2);
}

#[test]
fn drop_table_through_sql_with_if_exists() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE t (a text)").unwrap();
    db.query("DROP TABLE t").unwrap();
    assert!(db.query("DROP TABLE t").is_err());
    db.query("DROP TABLE IF EXISTS t").unwrap();
    db.query("DROP TABLE IF EXISTS never_existed").unwrap();
}

// --- DROP INDEX ---------------------------------------------------------------

#[test]
fn drop_index_stops_enforcing_and_still_answers_queries() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    db.create_index("users", "name", true).unwrap();
    insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    assert!(matches!(
        db.insert("users", {
            let mut r = Record::new();
            r.insert("name".into(), Value::Text("ana".into()));
            r
        }),
        Err(Error::UniqueViolation { .. })
    ));

    db.drop_index("users", "name").unwrap();
    assert!(db.table_schema("users").unwrap().indexes.is_empty());
    assert!(matches!(
        db.drop_index("users", "name"),
        Err(Error::IndexNotFound { .. })
    ));
    // Without the index the duplicate is accepted, and lookups fall back to a
    // scan that still finds both rows.
    insert(&db, "users", &[("name", Value::Text("ana".into()))]);
    assert_eq!(
        db.find_eq("users", "name", &Value::Text("ana".into()))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        rows(db.query("SELECT id FROM users WHERE name = 'ana'").unwrap()).len(),
        2
    );
    drop(db);

    let db = Db::open(&path).unwrap();
    assert!(db.table_schema("users").unwrap().indexes.is_empty());
    assert_eq!(
        db.find_eq("users", "name", &Value::Text("ana".into()))
            .unwrap()
            .len(),
        2
    );
    assert_clean(&path);
}

#[test]
fn drop_vector_and_text_indexes() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("body", ColumnType::Text),
            Column::vector("embedding", 3),
        ],
    ))
    .unwrap();
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
        .unwrap();
    db.create_text_index("docs", "body").unwrap();
    insert(
        &db,
        "docs",
        &[
            ("body", Value::Text("the quick brown fox".into())),
            ("embedding", Value::Vector(vec![1.0, 0.0, 0.0])),
        ],
    );
    db.wait_vector_indexing().unwrap();
    assert_eq!(
        db.search_text("docs", "body", "fox", 5, None)
            .unwrap()
            .len(),
        1
    );
    let vidx_files = |p: &Path| {
        fs::read_dir(p.join("vectors"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "vidx"))
                    .count()
            })
            .unwrap_or(0)
    };

    db.drop_text_index("docs", "body").unwrap();
    assert!(db.search_text("docs", "body", "fox", 5, None).is_err());
    assert!(matches!(
        db.drop_text_index("docs", "body"),
        Err(Error::IndexNotFound { .. })
    ));

    db.drop_vector_index("docs", "embedding").unwrap();
    assert!(db
        .search_vector(
            "docs",
            "embedding",
            &[1.0, 0.0, 0.0],
            5,
            &VectorSearchOptions::default()
        )
        .is_err());
    assert_eq!(
        vidx_files(&path),
        0,
        "the persisted graph should be deleted"
    );

    // The column and its data survive; only the index is gone.
    let schema = db.table_schema("docs").unwrap();
    assert!(schema.vector_indexes.is_empty() && schema.text_indexes.is_empty());
    assert_eq!(schema.columns.len(), 2);
    assert_eq!(db.scan("docs").unwrap().len(), 1);
    drop(db);
    let db = Db::open(&path).unwrap();
    let schema = db.table_schema("docs").unwrap();
    assert!(schema.vector_indexes.is_empty() && schema.text_indexes.is_empty());
    assert_eq!(db.scan("docs").unwrap().len(), 1);
    assert_clean(&path);
}

#[test]
fn drop_index_through_sql() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE t (a text)").unwrap();
    db.query("CREATE UNIQUE INDEX idx_a ON t (a)").unwrap();
    db.query("DROP INDEX idx_a ON t (a)").unwrap();
    assert!(db.query("DROP INDEX ON t (a)").is_err());
    db.query("DROP INDEX IF EXISTS ON t (a)").unwrap();
    db.query("CREATE INDEX ON t (a)").unwrap();
    db.query("DROP INDEX ON t (a)").unwrap();
    assert!(db.table_schema("t").unwrap().indexes.is_empty());
}

// --- ALTER TABLE ADD COLUMN ---------------------------------------------------

#[test]
fn add_column_is_metadata_only_and_reads_null() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );

    db.query("ALTER TABLE users ADD COLUMN email text").unwrap();
    let read = db.get("users", &id).unwrap().unwrap();
    assert_eq!(
        read.get("email"),
        None,
        "no bytes are written for old records"
    );
    let out = db.query("SELECT * FROM users").unwrap();
    assert_eq!(headers(out.clone()), vec!["id", "name", "age", "email"]);
    assert_eq!(
        rows(out)[0][3],
        Value::Null,
        "an absent column reads as NULL"
    );

    // New writes carry it.
    db.query("INSERT INTO users (name, email) VALUES ('bob', 'b@x.io')")
        .unwrap();
    let bob = rows(
        db.query("SELECT email FROM users WHERE name = 'bob'")
            .unwrap(),
    );
    assert_eq!(bob[0][0], Value::Text("b@x.io".into()));
    // And it is a real column: unknown ones are still rejected.
    assert!(db
        .query("INSERT INTO users (name, nope) VALUES ('x', 'y')")
        .is_err());
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(db.table_schema("users").unwrap().columns.len(), 3);
    assert_eq!(
        rows(
            db.query("SELECT email FROM users WHERE name = 'ana'")
                .unwrap()
        )[0][0],
        Value::Null
    );
    assert_clean(&path);
}

#[test]
fn add_column_with_default_backfills_existing_records() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    for i in 0..1200 {
        insert(&db, "users", &[("name", Value::Text(format!("u{i}")))]);
    }
    db.checkpoint().unwrap();

    db.query("ALTER TABLE users ADD COLUMN plan text NOT NULL DEFAULT 'free'")
        .unwrap();
    let all = db.scan("users").unwrap();
    assert_eq!(all.len(), 1200);
    assert!(
        all.iter()
            .all(|(_, r)| r["plan"] == Value::Text("free".into())),
        "every record should be backfilled"
    );
    // The default applies to later writes too, and NOT NULL is now enforced.
    db.query("INSERT INTO users (name) VALUES ('new')").unwrap();
    assert_eq!(
        rows(
            db.query("SELECT plan FROM users WHERE name = 'new'")
                .unwrap()
        )[0][0],
        Value::Text("free".into())
    );
    assert!(db
        .query("INSERT INTO users (name, plan) VALUES ('bad', NULL)")
        .is_err());
    let col = db
        .table_schema("users")
        .unwrap()
        .columns
        .into_iter()
        .find(|c| c.name == "plan")
        .unwrap();
    assert!(!col.nullable);
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.scan("users")
            .unwrap()
            .iter()
            .filter(|(_, r)| r["plan"] == Value::Text("free".into()))
            .count(),
        1201
    );
    assert_clean(&path);
}

#[test]
fn add_column_not_null_without_default_is_refused_on_a_non_empty_table() {
    let (_d, db) = new_db();
    db.create_table(users()).unwrap();
    // Allowed while the table is still empty.
    db.query("ALTER TABLE users ADD COLUMN a int NOT NULL DEFAULT 1")
        .unwrap();
    db.query("ALTER TABLE users ADD COLUMN b int NOT NULL")
        .unwrap();
    insert(
        &db,
        "users",
        &[("name", Value::Text("ana".into())), ("b", Value::Int64(7))],
    );
    let err = db
        .query("ALTER TABLE users ADD COLUMN c int NOT NULL")
        .unwrap_err();
    assert!(
        err.to_string().contains("DEFAULT"),
        "the error should point at DEFAULT: {err}"
    );
    // Duplicates and reserved names are refused.
    assert!(db.query("ALTER TABLE users ADD COLUMN name text").is_err());
    assert!(db.query("ALTER TABLE users ADD COLUMN id text").is_err());
    assert!(db.query("ALTER TABLE nope ADD COLUMN x int").is_err());
    // A default that does not match the column type is refused.
    assert!(db
        .query("ALTER TABLE users ADD COLUMN d int DEFAULT 'x'")
        .is_err());
}

#[test]
fn add_column_default_feeds_indexes() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    insert(&db, "users", &[("name", Value::Text("ana".into()))]);
    db.query("ALTER TABLE users ADD COLUMN plan text DEFAULT 'free'")
        .unwrap();
    db.create_index("users", "plan", false).unwrap();
    insert(&db, "users", &[("name", Value::Text("bob".into()))]);
    assert_eq!(
        db.find_eq("users", "plan", &Value::Text("free".into()))
            .unwrap()
            .len(),
        2,
        "the backfilled and the defaulted record must both be indexed"
    );
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.find_eq("users", "plan", &Value::Text("free".into()))
            .unwrap()
            .len(),
        2
    );
}

// --- ALTER TABLE DROP COLUMN --------------------------------------------------

#[test]
fn drop_column_removes_data_and_its_index() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    db.create_index("users", "age", false).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    db.checkpoint().unwrap();

    db.query("ALTER TABLE users DROP COLUMN age").unwrap();
    let schema = db.table_schema("users").unwrap();
    assert_eq!(schema.columns.len(), 1);
    assert!(
        schema.indexes.is_empty(),
        "the index on the column goes with it"
    );
    let read = db.get("users", &id).unwrap().unwrap();
    assert_eq!(read.get("age"), None, "the value is gone from the payload");
    assert_eq!(read["name"], Value::Text("ana".into()));
    assert_eq!(
        headers(db.query("SELECT * FROM users").unwrap()),
        vec!["id", "name"]
    );
    assert!(db.query("SELECT age FROM users").is_err());
    assert!(db
        .query("INSERT INTO users (name, age) VALUES ('x', 1)")
        .is_err());
    assert!(matches!(
        db.drop_column("users", "age"),
        Err(Error::ColumnNotFound { .. })
    ));
    // The last column cannot be dropped: that would be a table drop.
    assert!(db.drop_column("users", "name").is_err());
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(db.table_schema("users").unwrap().columns.len(), 1);
    assert_eq!(db.get("users", &id).unwrap().unwrap().get("age"), None);
    assert_clean(&path);
    db.query("ALTER TABLE users DROP COLUMN IF EXISTS age")
        .unwrap();
}

#[test]
fn drop_column_keeps_other_columns_including_blobs_and_vectors() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text),
            Column::new("payload", ColumnType::Blob),
            Column::new("scratch", ColumnType::Int64),
            Column::vector("embedding", 3),
        ],
    ))
    .unwrap();
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
        .unwrap();
    // Large enough to be stored out of line, so the rewrite must keep the
    // blob reference pointing at its chunk.
    let big = vec![7u8; 512 * 1024];
    let id = insert(
        &db,
        "docs",
        &[
            ("title", Value::Text("t".into())),
            ("payload", Value::Blob(big.clone())),
            ("scratch", Value::Int64(1)),
            ("embedding", Value::Vector(vec![0.0, 1.0, 0.0])),
        ],
    );
    db.wait_vector_indexing().unwrap();

    db.drop_column("docs", "scratch").unwrap();
    let read = db.get("docs", &id).unwrap().unwrap();
    assert_eq!(read.get("scratch"), None);
    assert_eq!(read["payload"], Value::Blob(big.clone()));
    assert_eq!(read["embedding"], Value::Vector(vec![0.0, 1.0, 0.0]));
    let hits = db
        .search_vector(
            "docs",
            "embedding",
            &[0.0, 1.0, 0.0],
            5,
            &VectorSearchOptions::default(),
        )
        .unwrap();
    assert_eq!(hits.len(), 1, "the ANN index survives the rewrite");
    drop(db);

    let db = Db::open(&path).unwrap();
    let read = db.get("docs", &id).unwrap().unwrap();
    assert_eq!(read["payload"], Value::Blob(big));
    assert_clean(&path);
}

// --- ALTER TABLE RENAME -------------------------------------------------------

#[test]
fn rename_table_moves_records_and_indexes() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    db.create_index("users", "name", true).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    insert(
        &db,
        "users",
        &[
            ("name", Value::Text("bob".into())),
            ("age", Value::Int64(41)),
        ],
    );
    db.checkpoint().unwrap();

    db.query("ALTER TABLE users RENAME TO people").unwrap();
    assert_eq!(db.tables(), vec!["people".to_string()]);
    assert!(matches!(db.scan("users"), Err(Error::TableNotFound(_))));
    assert_eq!(db.scan("people").unwrap().len(), 2);
    assert_eq!(
        db.get("people", &id).unwrap().unwrap()["name"],
        Value::Text("ana".into())
    );
    assert_eq!(
        db.find_eq("people", "name", &Value::Text("bob".into()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        rows(db.query("SELECT name FROM people ORDER BY name").unwrap()).len(),
        2
    );
    // The unique index moved with the table.
    assert!(matches!(
        db.insert("people", {
            let mut r = Record::new();
            r.insert("name".into(), Value::Text("ana".into()));
            r
        }),
        Err(Error::UniqueViolation { .. })
    ));
    // Renaming onto an existing name is refused.
    db.create_table(TableSchema::new(
        "other",
        vec![Column::new("x", ColumnType::Text)],
    ))
    .unwrap();
    assert!(matches!(
        db.rename_table("people", "other"),
        Err(Error::TableExists(_))
    ));
    assert!(matches!(
        db.rename_table("ghost", "x"),
        Err(Error::TableNotFound(_))
    ));
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("people").unwrap().len(), 2);
    assert_eq!(
        db.get("people", &id).unwrap().unwrap()["age"],
        Value::Int64(30)
    );
    assert_clean(&path);
    // Writes keep working after the reopen, and so does the moved index.
    insert(&db, "people", &[("name", Value::Text("cleo".into()))]);
    assert_eq!(db.scan("people").unwrap().len(), 3);
}

#[test]
fn rename_column_carries_values_and_indexes() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    db.create_index("users", "name", true).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    db.checkpoint().unwrap();

    db.query("ALTER TABLE users RENAME COLUMN name TO full_name")
        .unwrap();
    let schema = db.table_schema("users").unwrap();
    assert!(schema.column("full_name").is_some() && schema.column("name").is_none());
    assert_eq!(schema.indexes[0].column, "full_name");
    let read = db.get("users", &id).unwrap().unwrap();
    assert_eq!(read["full_name"], Value::Text("ana".into()));
    assert_eq!(read.get("name"), None);
    assert_eq!(read["age"], Value::Int64(30));
    assert_eq!(
        rows(
            db.query("SELECT full_name FROM users WHERE full_name = 'ana'")
                .unwrap()
        )
        .len(),
        1
    );
    assert!(db.query("SELECT name FROM users").is_err());
    // The unique index followed the rename.
    assert!(matches!(
        db.insert("users", {
            let mut r = Record::new();
            r.insert("full_name".into(), Value::Text("ana".into()));
            r
        }),
        Err(Error::UniqueViolation { .. })
    ));
    assert!(matches!(
        db.rename_column("users", "ghost", "x"),
        Err(Error::ColumnNotFound { .. })
    ));
    assert!(db.rename_column("users", "age", "full_name").is_err());
    assert!(db.rename_column("users", "age", "id").is_err());
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.get("users", &id).unwrap().unwrap()["full_name"],
        Value::Text("ana".into())
    );
    assert_eq!(
        db.find_eq("users", "full_name", &Value::Text("ana".into()))
            .unwrap()
            .len(),
        1
    );
    assert_clean(&path);
}

#[test]
fn renames_keep_vector_and_text_search_working() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("body", ColumnType::Text),
            Column::vector("embedding", 3),
        ],
    ))
    .unwrap();
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
        .unwrap();
    db.create_text_index("docs", "body").unwrap();
    insert(
        &db,
        "docs",
        &[
            ("body", Value::Text("the quick brown fox".into())),
            ("embedding", Value::Vector(vec![1.0, 0.0, 0.0])),
        ],
    );
    db.wait_vector_indexing().unwrap();

    db.rename_column("docs", "body", "text_body").unwrap();
    db.rename_table("docs", "documents").unwrap();
    assert_eq!(
        db.search_text("documents", "text_body", "fox", 5, None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.search_vector(
            "documents",
            "embedding",
            &[1.0, 0.0, 0.0],
            5,
            &VectorSearchOptions::default()
        )
        .unwrap()
        .len(),
        1
    );
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.search_text("documents", "text_body", "fox", 5, None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.search_vector(
            "documents",
            "embedding",
            &[1.0, 0.0, 0.0],
            5,
            &VectorSearchOptions::default()
        )
        .unwrap()
        .len(),
        1
    );
    assert_clean(&path);
}

// --- crash recovery -----------------------------------------------------------

/// The states a crash can leave a data-touching DDL in are exactly "the intent
/// is on disk and some prefix of its steps ran". Each case below reconstructs
/// one of them and checks that opening the database finishes the job.

#[test]
fn interrupted_rename_table_is_completed_on_open() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    db.checkpoint().unwrap();
    drop(db);

    // Crashed right after recording the intent: nothing else ran.
    write_intent(
        &path,
        r#"{"op":"RenameTable","table":"users","to":"people"}"#,
    );
    let report = check(&path).unwrap();
    assert!(report.is_ok());
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("RENAME TO people")));

    let db = Db::open(&path).unwrap();
    assert_eq!(db.tables(), vec!["people".to_string()]);
    assert_eq!(
        db.get("people", &id).unwrap().unwrap()["name"],
        Value::Text("ana".into())
    );
    assert!(
        !path.join("ddl.json").exists(),
        "the record must be cleared"
    );
    drop(db);
    assert_clean(&path);

    // Crashed after the data and catalog were both published: replaying must
    // be a no-op, not a second rename.
    write_intent(
        &path,
        r#"{"op":"RenameTable","table":"users","to":"people"}"#,
    );
    let db = Db::open(&path).unwrap();
    assert_eq!(db.tables(), vec!["people".to_string()]);
    assert_eq!(db.scan("people").unwrap().len(), 1);
}

#[test]
fn interrupted_rename_table_with_data_already_moved_is_completed_on_open() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    // Move the data (and the catalog) with a real rename, then rewind only the
    // catalog: that is exactly the window between publishing the manifest and
    // publishing the catalog.
    db.rename_table("users", "people").unwrap();
    drop(db);
    let catalog = path.join("catalog.json");
    let text = fs::read_to_string(&catalog)
        .unwrap()
        .replace("\"people\"", "\"users\"");
    fs::write(&catalog, text).unwrap();
    write_intent(
        &path,
        r#"{"op":"RenameTable","table":"users","to":"people"}"#,
    );

    let db = Db::open(&path).unwrap();
    assert_eq!(db.tables(), vec!["people".to_string()]);
    let all = db.scan("people").unwrap();
    assert_eq!(all.len(), 1, "the moved records must not be discarded");
    assert_eq!(all[0].1["name"], Value::Text("ana".into()));
    assert_eq!(
        db.get("people", &id).unwrap().unwrap()["age"],
        Value::Int64(30)
    );
    drop(db);
    assert_clean(&path);
}

#[test]
fn interrupted_drop_column_and_rename_column_are_completed_on_open() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    let id = insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    db.checkpoint().unwrap();
    drop(db);

    write_intent(
        &path,
        r#"{"op":"RenameColumn","table":"users","column":"name","to":"full_name"}"#,
    );
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.get("users", &id).unwrap().unwrap()["full_name"],
        Value::Text("ana".into())
    );
    assert!(db.table_schema("users").unwrap().column("name").is_none());
    drop(db);

    write_intent(
        &path,
        r#"{"op":"DropColumn","table":"users","column":"age"}"#,
    );
    let db = Db::open(&path).unwrap();
    let read = db.get("users", &id).unwrap().unwrap();
    assert_eq!(read.get("age"), None);
    assert_eq!(read["full_name"], Value::Text("ana".into()));
    assert_eq!(db.table_schema("users").unwrap().columns.len(), 1);
    // Replaying either one again changes nothing.
    drop(db);
    write_intent(
        &path,
        r#"{"op":"DropColumn","table":"users","column":"age"}"#,
    );
    let db = Db::open(&path).unwrap();
    assert_eq!(db.table_schema("users").unwrap().columns.len(), 1);
    assert_eq!(db.scan("users").unwrap().len(), 1);
    drop(db);
    assert_clean(&path);
}

#[test]
fn interrupted_add_column_backfill_is_completed_on_open() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    for i in 0..30 {
        insert(&db, "users", &[("name", Value::Text(format!("u{i}")))]);
    }
    db.checkpoint().unwrap();
    drop(db);

    write_intent(
        &path,
        r#"{"op":"AddColumn","table":"users","column":{"name":"plan","type":"text","nullable":false,"default":"free"},"not_null":true}"#,
    );
    let db = Db::open(&path).unwrap();
    let all = db.scan("users").unwrap();
    assert_eq!(all.len(), 30);
    assert!(all
        .iter()
        .all(|(_, r)| r["plan"] == Value::Text("free".into())));
    let col = db
        .table_schema("users")
        .unwrap()
        .columns
        .into_iter()
        .find(|c| c.name == "plan")
        .unwrap();
    assert!(
        !col.nullable,
        "NOT NULL is applied once the backfill is done"
    );
    assert!(!path.join("ddl.json").exists());
    drop(db);
    assert_clean(&path);
}

#[test]
fn an_existing_ddl_intent_is_never_overwritten_by_a_new_schema_change() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    let original = r#"{"op":"RenameTable","table":"users","to":"people"}"#;
    write_intent(&path, original);

    assert!(matches!(
        db.rename_column("users", "name", "full_name"),
        Err(Error::CommitUnknown(_))
    ));
    assert_eq!(fs::read_to_string(path.join("ddl.json")).unwrap(), original);
    let mut refused = Record::new();
    refused.insert("name".into(), Value::Text("blocked".into()));
    assert!(matches!(
        db.insert("users", refused),
        Err(Error::CommitUnknown(_))
    ));
    drop(db);

    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.tables(), vec!["people".to_string()]);
    assert!(reopened
        .table_schema("people")
        .unwrap()
        .column("name")
        .is_some());
    assert!(!path.join("ddl.json").exists());
}

fn write_intent(db_path: &Path, json: &str) {
    fs::write(db_path.join("ddl.json"), json).unwrap();
}

// --- read-only ----------------------------------------------------------------

#[test]
fn ddl_is_rejected_read_only() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    db.create_index("users", "name", false).unwrap();
    insert(&db, "users", &[("name", Value::Text("ana".into()))]);
    drop(db);

    let db = Db::open_read_only(&path).unwrap();
    for result in [
        db.drop_table("users"),
        db.drop_index("users", "name"),
        db.drop_column("users", "age"),
        db.rename_table("users", "people"),
        db.rename_column("users", "name", "n"),
        db.add_column("users", Column::new("x", ColumnType::Int64)),
    ] {
        assert!(matches!(result, Err(Error::ReadOnly)));
    }
    assert!(matches!(db.query("DROP TABLE users"), Err(Error::ReadOnly)));
    assert_eq!(db.scan("users").unwrap().len(), 1);
}

// --- backup / salvage ---------------------------------------------------------

#[test]
fn backup_and_salvage_carry_the_altered_schema() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    insert(
        &db,
        "users",
        &[
            ("name", Value::Text("ana".into())),
            ("age", Value::Int64(30)),
        ],
    );
    db.create_table(TableSchema::new(
        "temp",
        vec![Column::new("x", ColumnType::Text)],
    ))
    .unwrap();
    insert(&db, "temp", &[("x", Value::Text("gone".into()))]);
    db.drop_table("temp").unwrap();
    db.query("ALTER TABLE users ADD COLUMN plan text NOT NULL DEFAULT 'free'")
        .unwrap();
    db.query("ALTER TABLE users RENAME COLUMN age TO years")
        .unwrap();
    db.query("ALTER TABLE users RENAME TO people").unwrap();

    let backup = dir.path().join("copy.esql");
    db.backup(&backup).unwrap();
    assert_clean(&backup);
    let copy = Db::open(&backup).unwrap();
    assert_eq!(copy.tables(), vec!["people".to_string()]);
    let row = copy.scan("people").unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].1["plan"], Value::Text("free".into()));
    assert_eq!(row[0].1["years"], Value::Int64(30));
    drop(copy);

    let salvaged = dir.path().join("salvaged.esql");
    assert!(matches!(
        elitesql_core::salvage(&path, &salvaged),
        Err(Error::DatabaseLocked(_))
    ));
    assert!(!salvaged.exists());
    drop(db);
    let report = elitesql_core::salvage(&path, &salvaged).unwrap();
    assert_eq!(report.tables, vec!["people".to_string()]);
    let out = Db::open(&salvaged).unwrap();
    let row = out.scan("people").unwrap();
    assert_eq!(row.len(), 1, "the dropped table must not come back");
    assert_eq!(row[0].1["years"], Value::Int64(30));
}

#[test]
fn salvage_does_not_resurrect_records_of_a_recreated_table() {
    let (dir, db) = new_db();
    let path = path_of(&dir);
    db.create_table(users()).unwrap();
    for i in 0..4 {
        insert(&db, "users", &[("name", Value::Text(format!("old{i}")))]);
    }
    db.checkpoint().unwrap();
    db.drop_table("users").unwrap();
    db.create_table(users()).unwrap();
    insert(&db, "users", &[("name", Value::Text("fresh".into()))]);
    db.checkpoint().unwrap();
    drop(db);

    let salvaged = dir.path().join("salvaged.esql");
    let report = elitesql_core::salvage(&path, &salvaged).unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("not owned by the catalog")),
        "the stale entries should be reported, not silently dropped: {:?}",
        report.notes
    );
    let out = Db::open(&salvaged).unwrap();
    let all = out.scan("users").unwrap();
    assert_eq!(
        all.len(),
        1,
        "records of the dropped table came back: {all:?}"
    );
    assert_eq!(all[0].1["name"], Value::Text("fresh".into()));
}
