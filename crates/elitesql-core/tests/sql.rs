//! sqllogictest-style suite for the SQL V1 subset: expected results for the
//! supported surface, and explicit clear errors for everything outside it.

use elitesql_core::{ColumnType, Db, Error, QueryOutput, Value};
use tempfile::TempDir;

fn new_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("sql.esql")).unwrap();
    (dir, db)
}

fn seeded() -> (TempDir, Db) {
    let (dir, db) = new_db();
    db.query("CREATE TABLE users (name text NOT NULL, email text, age int64)")
        .unwrap();
    db.query("CREATE UNIQUE INDEX ON users (email)").unwrap();
    db.query("CREATE TABLE orders (user_id text NOT NULL, amount int64, note text)")
        .unwrap();
    db.query("CREATE INDEX ON orders (user_id)").unwrap();
    db.query(
        "INSERT INTO users (id, name, email, age) VALUES \
         ('u1', 'ana', 'ana@x.com', 30), \
         ('u2', 'bob', 'bob@x.com', 25), \
         ('u3', 'eva', NULL, 41)",
    )
    .unwrap();
    db.query(
        "INSERT INTO orders (user_id, amount, note) VALUES \
         ('u1', 100, 'first'), \
         ('u1', 250, 'second'), \
         ('u2', 75, 'only'), \
         ('u9', 999, 'orphan')",
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

fn texts(vals: &[Vec<Value>], col: usize) -> Vec<String> {
    vals.iter()
        .map(|r| match &r[col] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

fn assert_sql_err(db: &Db, sql: &str, needle: &str) {
    match db.query(sql) {
        Err(Error::Sql(msg)) => assert!(
            msg.to_lowercase().contains(&needle.to_lowercase()),
            "for {sql:?}: expected error containing {needle:?}, got {msg:?}"
        ),
        Err(other) => panic!("for {sql:?}: expected Sql error, got {other:?}"),
        Ok(out) => panic!("for {sql:?}: expected error, got {out:?}"),
    }
}

// --- DDL -----------------------------------------------------------------------

#[test]
fn create_table_types_and_errors() {
    let (_d, db) = new_db();
    db.query(
        "CREATE TABLE t (b bool, i int, f float64, s text NOT NULL, \
         bl blob, ts timestamp, j json)",
    )
    .unwrap();
    assert!(db.tables().contains(&"t".to_string()));

    db.query("CREATE TABLE int_aliases (a integer, b bigint, c int64)")
        .unwrap();
    let aliases = db.table_schema("int_aliases").unwrap();
    assert!(aliases
        .columns
        .iter()
        .all(|column| column.ty == ColumnType::Int64));

    assert_sql_err(&db, "CREATE TABLE bad (x smallint)", "use int");
    assert_sql_err(&db, "CREATE TABLE bad (x int32)", "use int");
    assert_sql_err(&db, "CREATE TABLE bad (x varchar)", "use text");
    assert_sql_err(&db, "CREATE TABLE bad (x double)", "use float64");
    assert_sql_err(&db, "CREATE TABLE bad (x datetime)", "use timestamp");
    assert_sql_err(&db, "CREATE TABLE bad (x whatever)", "V1 types are");
    assert_sql_err(
        &db,
        "CREATE TABLE bad (x text PRIMARY KEY)",
        "implicit 'id'",
    );
    assert_sql_err(
        &db,
        "CREATE TABLE bad (x text DEFAULT 'y' DEFAULT 'z')",
        "duplicate DEFAULT",
    );
    assert_sql_err(
        &db,
        "CREATE TABLE bad (x int DEFAULT 'y')",
        "not valid for column",
    );
    assert_sql_err(
        &db,
        "CREATE TABLE bad (x text UNIQUE)",
        "CREATE UNIQUE INDEX",
    );
}

#[test]
fn create_index_variants() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE t (a text, b int64)").unwrap();
    db.query("CREATE INDEX ON t (a)").unwrap();
    db.query("CREATE UNIQUE INDEX idx_b ON t (b)").unwrap();
    assert_sql_err(&db, "CREATE INDEX ON t (a, b)", "multi-column");
    assert!(matches!(
        db.query("CREATE INDEX ON t (nope)"),
        Err(Error::SchemaViolation(_))
    ));
}

// --- INSERT --------------------------------------------------------------------

#[test]
fn insert_returns_ids_and_is_atomic() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE t (n int64 NOT NULL)").unwrap();
    let out = db.query("INSERT INTO t (n) VALUES (1), (2), (3)").unwrap();
    let QueryOutput::Inserted { ids } = out else {
        panic!("expected Inserted")
    };
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|id| id.len() == 26), "generated ULIDs");

    // A bad row anywhere aborts the whole INSERT (single transaction).
    let before = rows(db.query("SELECT * FROM t").unwrap()).1.len();
    assert!(db.query("INSERT INTO t (n) VALUES (4), (NULL)").is_err());
    let after = rows(db.query("SELECT * FROM t").unwrap()).1.len();
    assert_eq!(before, after, "failed INSERT must not leave partial rows");

    let out = db.query("INSERT INTO t VALUES (4), (5)").unwrap();
    let QueryOutput::Inserted { ids } = out else {
        panic!("expected Inserted")
    };
    assert_eq!(ids.len(), 2, "omitted column list uses declaration order");
    assert_sql_err(&db, "INSERT INTO t VALUES (6, 7)", "1 declared columns");
}

#[test]
fn insert_coercions_and_blob() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE t (f float64, ts timestamp, j json, b blob)")
        .unwrap();
    db.query(
        "INSERT INTO t (f, ts, j, b) VALUES (3, 1722000000000000, '{\"k\": [1, 2]}', X'DEADBEEF')",
    )
    .unwrap();
    let (_, r) = rows(db.query("SELECT f, ts, j, b FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Float64(3.0),
        "int literal coerced to float64"
    );
    assert_eq!(r[0][1], Value::Timestamp(1_722_000_000_000_000));
    assert_eq!(r[0][2], Value::Json(serde_json::json!({"k": [1, 2]})));
    assert_eq!(r[0][3], Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]));

    assert_sql_err(&db, "INSERT INTO t (j) VALUES ('not json')", "invalid json");
    assert_sql_err(
        &db,
        "INSERT INTO t (f) VALUES ('text')",
        "not valid for column",
    );
}

// --- SELECT --------------------------------------------------------------------

#[test]
fn select_where_order_limit_offset() {
    let (_d, db) = seeded();
    let (cols, r) = rows(
        db.query("SELECT name, age FROM users WHERE age >= 25 ORDER BY age DESC")
            .unwrap(),
    );
    assert_eq!(cols, vec!["name", "age"]);
    assert_eq!(texts(&r, 0), vec!["eva", "ana", "bob"]);

    let (_, r) = rows(
        db.query("SELECT name FROM users ORDER BY age ASC LIMIT 1 OFFSET 1")
            .unwrap(),
    );
    assert_eq!(texts(&r, 0), vec!["ana"]);

    let (_, r) = rows(db.query("SELECT name FROM users WHERE age > 100").unwrap());
    assert!(r.is_empty());

    let (_, r) = rows(db.query("SELECT name FROM users LIMIT 0").unwrap());
    assert!(r.is_empty());
}

#[test]
fn select_predicates() {
    let (_d, db) = seeded();
    let q = |sql: &str| texts(&rows(db.query(sql).unwrap()).1, 0);

    assert_eq!(q("SELECT name FROM users WHERE name = 'ana'"), vec!["ana"]);
    assert_eq!(
        q("SELECT name FROM users WHERE age > 24 AND age < 35 ORDER BY name"),
        vec!["ana", "bob"]
    );
    assert_eq!(
        q("SELECT name FROM users WHERE age = 30 OR age = 41 ORDER BY name"),
        vec!["ana", "eva"]
    );
    assert_eq!(
        q("SELECT name FROM users WHERE NOT age = 30 ORDER BY name"),
        vec!["bob", "eva"]
    );
    assert_eq!(q("SELECT name FROM users WHERE email IS NULL"), vec!["eva"]);
    assert_eq!(
        q("SELECT name FROM users WHERE email IS NOT NULL ORDER BY name"),
        vec!["ana", "bob"]
    );
    assert_eq!(
        q("SELECT name FROM users WHERE age IN (25, 41) ORDER BY name"),
        vec!["bob", "eva"]
    );
    assert_eq!(
        q("SELECT name FROM users WHERE age NOT IN (25, 41)"),
        vec!["ana"]
    );
    assert_eq!(
        q("SELECT name FROM users WHERE (age = 25 OR age = 30) AND name = 'bob'"),
        vec!["bob"]
    );
    // Comparisons with NULL literals are false (two-valued logic).
    assert!(q("SELECT name FROM users WHERE email = NULL").is_empty());
    // Point lookup by id.
    assert_eq!(q("SELECT name FROM users WHERE id = 'u2'"), vec!["bob"]);
    // Indexed lookup by unique email.
    assert_eq!(
        q("SELECT name FROM users WHERE email = 'ana@x.com'"),
        vec!["ana"]
    );
}

#[test]
fn line_and_block_comments_are_ignored() {
    let (_d, db) = seeded();
    let (_, result) = rows(
        db.query(
            "/* leading ; comment */ SELECT name -- middle ; comment\n\
             FROM users WHERE age = 30;",
        )
        .unwrap(),
    );
    assert_eq!(texts(&result, 0), ["ana"]);
    assert_sql_err(
        &db,
        "SELECT name FROM users /* unfinished",
        "unterminated block comment",
    );
}

#[test]
fn select_star_and_aliases() {
    let (_d, db) = seeded();
    let (cols, _) = rows(db.query("SELECT * FROM users LIMIT 1").unwrap());
    assert_eq!(cols, vec!["id", "name", "email", "age"]);

    let (cols, r) = rows(
        db.query("SELECT name AS who, age AS years FROM users WHERE id = 'u1'")
            .unwrap(),
    );
    assert_eq!(cols, vec!["who", "years"]);
    assert_eq!(r[0][1], Value::Int64(30));
}

// --- JOINS ---------------------------------------------------------------------

#[test]
fn inner_join_with_index_and_pushdown() {
    let (_d, db) = seeded();
    let (cols, r) = rows(
        db.query(
            "SELECT u.name, o.amount FROM users u \
             INNER JOIN orders o ON o.user_id = u.id \
             WHERE u.email = 'ana@x.com' ORDER BY o.amount",
        )
        .unwrap(),
    );
    assert_eq!(cols, vec!["name", "amount"]);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Int64(100));
    assert_eq!(r[1][1], Value::Int64(250));
}

#[test]
fn left_join_preserves_unmatched_left() {
    let (_d, db) = seeded();
    let (_, r) = rows(
        db.query(
            "SELECT u.name, o.amount FROM users u \
             LEFT JOIN orders o ON o.user_id = u.id ORDER BY u.name, o.amount",
        )
        .unwrap(),
    );
    // ana x2, bob x1, eva with NULL amount.
    assert_eq!(r.len(), 4);
    assert_eq!(texts(&r, 0), vec!["ana", "ana", "bob", "eva"]);
    assert_eq!(r[3][1], Value::Null, "eva has no orders");
}

#[test]
fn right_join_preserves_unmatched_right() {
    let (_d, db) = seeded();
    let (_, r) = rows(
        db.query(
            "SELECT u.name, o.note FROM users u \
             RIGHT JOIN orders o ON o.user_id = u.id ORDER BY o.note",
        )
        .unwrap(),
    );
    // 4 orders; the 'orphan' one has no user.
    assert_eq!(r.len(), 4);
    let orphan = r
        .iter()
        .find(|row| row[1] == Value::Text("orphan".into()))
        .unwrap();
    assert_eq!(orphan[0], Value::Null, "orphan order keeps NULL user side");
}

#[test]
fn join_star_qualifies_headers() {
    let (_d, db) = seeded();
    let (cols, _) = rows(
        db.query("SELECT * FROM users u JOIN orders o ON o.user_id = u.id LIMIT 1")
            .unwrap(),
    );
    assert_eq!(
        cols,
        vec![
            "u.id",
            "u.name",
            "u.email",
            "u.age",
            "o.id",
            "o.user_id",
            "o.amount",
            "o.note"
        ]
    );
}

#[test]
fn chained_joins() {
    let (_d, db) = seeded();
    db.query("CREATE TABLE tags (order_id text NOT NULL, tag text)")
        .unwrap();
    db.query("CREATE INDEX ON tags (order_id)").unwrap();
    // Tag ana's 100 order.
    let (_, r) = rows(
        db.query("SELECT id FROM orders WHERE amount = 100")
            .unwrap(),
    );
    let oid = match &r[0][0] {
        Value::Text(s) => s.clone(),
        _ => unreachable!(),
    };
    db.query(&format!(
        "INSERT INTO tags (order_id, tag) VALUES ('{oid}', 'vip')"
    ))
    .unwrap();

    let (_, r) = rows(
        db.query(
            "SELECT u.name, o.amount, t.tag FROM users u \
             JOIN orders o ON o.user_id = u.id \
             JOIN tags t ON t.order_id = o.id",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("ana".into()));
    assert_eq!(r[0][2], Value::Text("vip".into()));
}

#[test]
fn ambiguous_column_requires_qualification() {
    let (_d, db) = seeded();
    assert_sql_err(
        &db,
        "SELECT id FROM users u JOIN orders o ON o.user_id = u.id",
        "ambiguous",
    );
}

// --- UPDATE / DELETE -------------------------------------------------------------

#[test]
fn update_with_where_reports_count() {
    let (_d, db) = seeded();
    let out = db
        .query("UPDATE users SET age = 26 WHERE name = 'bob'")
        .unwrap();
    assert_eq!(out, QueryOutput::Affected(1));
    let (_, r) = rows(db.query("SELECT age FROM users WHERE id = 'u2'").unwrap());
    assert_eq!(r[0][0], Value::Int64(26));

    let out = db.query("UPDATE users SET age = 0").unwrap();
    assert_eq!(out, QueryOutput::Affected(3), "no WHERE = all rows");

    assert_sql_err(&db, "UPDATE users SET id = 'nope'", "primary key");
    assert_sql_err(
        &db,
        "UPDATE users SET age = age",
        "SET only accepts literal",
    );
}

#[test]
fn delete_with_where_reports_count() {
    let (_d, db) = seeded();
    let out = db.query("DELETE FROM orders WHERE amount < 100").unwrap();
    assert_eq!(out, QueryOutput::Affected(1));
    let (_, r) = rows(db.query("SELECT * FROM orders").unwrap());
    assert_eq!(r.len(), 3);

    let out = db.query("DELETE FROM orders").unwrap();
    assert_eq!(out, QueryOutput::Affected(3));
    assert!(rows(db.query("SELECT * FROM orders").unwrap()).1.is_empty());
}

#[test]
fn unindexed_equality_pushdown_drives_select_update_and_delete_after_checkpoint() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE jobs (name text NOT NULL, score int)")
        .unwrap();
    db.query(
        "INSERT INTO jobs (id, name, score) VALUES \
         ('a', 'first', 7), ('b', 'other', 2), ('c', 'third', 7)",
    )
    .unwrap();
    db.checkpoint().unwrap();

    let (_, selected) = rows(
        db.query("SELECT id, name FROM jobs WHERE score = 7 ORDER BY id")
            .unwrap(),
    );
    assert_eq!(texts(&selected, 0), ["a", "c"]);

    assert_eq!(
        db.query("UPDATE jobs SET name = 'matched' WHERE score = 7")
            .unwrap(),
        QueryOutput::Affected(2)
    );
    assert_eq!(
        db.query("DELETE FROM jobs WHERE score = 2").unwrap(),
        QueryOutput::Affected(1)
    );
    let (_, remaining) = rows(db.query("SELECT id, name FROM jobs ORDER BY id").unwrap());
    assert_eq!(texts(&remaining, 0), ["a", "c"]);
    assert_eq!(texts(&remaining, 1), ["matched", "matched"]);
}

#[test]
fn sql_respects_unique_index() {
    let (_d, db) = seeded();
    let err = db
        .query("INSERT INTO users (name, email) VALUES ('clone', 'ana@x.com')")
        .unwrap_err();
    assert!(matches!(err, Error::UniqueViolation { .. }));
}

// --- out-of-subset rejections ---------------------------------------------------

#[test]
fn unsupported_features_fail_with_clear_errors() {
    let (_d, db) = seeded();
    assert_sql_err(
        &db,
        "SELECT * FROM users FULL OUTER JOIN orders ON o.a = u.b",
        "FULL OUTER",
    );
    assert_sql_err(&db, "SELECT * FROM users CROSS JOIN orders", "CROSS JOIN");
    assert_sql_err(&db, "SELECT * FROM (SELECT * FROM users)", "subqueries");
    assert_sql_err(
        &db,
        "SELECT * FROM users WHERE id IN (SELECT id FROM users)",
        "subqueries",
    );
    assert_sql_err(
        &db,
        "SELECT * FROM users WHERE (SELECT 1) = 1",
        "subqueries",
    );
    assert_sql_err(&db, "WITH x AS (SELECT 1) SELECT * FROM x", "CTEs");
    // Aggregates exist since Phase 2.5, but only in SELECT and HAVING.
    assert_sql_err(&db, "SELECT name FROM users WHERE COUNT(*) > 1", "HAVING");
    assert_sql_err(
        &db,
        "SELECT COUNT(DISTINCT name) FROM users",
        "DISTINCT inside aggregates",
    );
    assert_sql_err(&db, "SELECT SUM(*) FROM users", "only COUNT accepts *");
    assert_sql_err(
        &db,
        "SELECT name FROM users UNION SELECT name FROM users",
        "UNION",
    );
    assert_sql_err(&db, "SELECT DISTINCT name FROM users", "DISTINCT");
    assert_sql_err(&db, "SELECT age + 1 FROM users", "arithmetic");
    assert_sql_err(
        &db,
        "SELECT name FROM users WHERE age + 1 = 31",
        "arithmetic",
    );
    assert_sql_err(&db, "SELECT name FROM users WHERE name LIKE 'a%'", "LIKE");
    assert_sql_err(
        &db,
        "SELECT name FROM users ORDER BY lower(name)",
        "use search_vector",
    );
    assert_sql_err(
        &db,
        "SELECT name FROM users WHERE age BETWEEN 20 AND 30",
        "BETWEEN",
    );
    assert_sql_err(&db, "DROP DATABASE app", "delete the database directory");
    assert_sql_err(&db, "DROP VIEW v", "views");
    assert_sql_err(&db, "DROP INDEX idx", "DROP INDEX ON table (column)");
    assert_sql_err(&db, "ALTER TABLE users", "expected ADD, DROP or RENAME");
    assert_sql_err(
        &db,
        "ALTER TABLE users ALTER COLUMN age TYPE text",
        "not supported",
    );
    assert_sql_err(&db, "ALTER INDEX i RENAME TO j", "only ALTER TABLE");
    assert_sql_err(&db, "DROP TABLE users CASCADE", "CASCADE");
    assert_sql_err(&db, "CREATE VIEW v AS SELECT 1", "views");
    assert_sql_err(&db, "CREATE TRIGGER tr", "triggers");
    assert_sql_err(&db, "BEGIN", "Txn API");
    assert_sql_err(&db, "SELECT u.* FROM users u", "qualified star");
    assert_sql_err(
        &db,
        "SELECT * FROM users u JOIN orders o ON o.user_id = u.id AND o.amount > 5",
        "single column equality",
    );
    assert_sql_err(
        &db,
        "SELECT name FROM users; SELECT name FROM users",
        "one statement",
    );
    assert_sql_err(&db, "INSERT INTO users VALUES ('x')", "3 declared columns");
    assert_sql_err(&db, "SELECT nope FROM users", "unknown column");
    assert!(matches!(
        db.query("SELECT * FROM missing"),
        Err(Error::TableNotFound(_))
    ));
}

// --- consistency ------------------------------------------------------------------

#[test]
fn sql_and_api_interoperate() {
    let (_d, db) = seeded();
    // SQL sees API writes and vice versa.
    let mut rec = elitesql_core::Record::new();
    rec.insert("name".into(), Value::Text("api-user".into()));
    rec.insert("age".into(), Value::Int64(50));
    let id = db.insert("users", rec).unwrap();

    let (_, r) = rows(db.query("SELECT name FROM users WHERE age = 50").unwrap());
    assert_eq!(texts(&r, 0), vec!["api-user"]);

    db.query(&format!("UPDATE users SET age = 51 WHERE id = '{id}'"))
        .unwrap();
    let rec = db.get("users", &id).unwrap().unwrap();
    assert_eq!(rec["age"], Value::Int64(51));
}

#[test]
fn sql_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sql.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE kv (v text)").unwrap();
        db.query("INSERT INTO kv (id, v) VALUES ('k1', 'hello')")
            .unwrap();
    }
    let db = Db::open(&path).unwrap();
    let (_, r) = rows(db.query("SELECT v FROM kv WHERE id = 'k1'").unwrap());
    assert_eq!(r[0][0], Value::Text("hello".into()));
}
