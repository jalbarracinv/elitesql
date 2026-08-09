//! ORDER BY collation.
//!
//! UTF-8 byte order is not alphabetical order: `ñ` and every accented vowel
//! encode above `z`, and uppercase ASCII encodes below lowercase. ORDER BY on
//! text therefore collates by default, and `COLLATE binary` asks for the raw
//! byte order back.

use elitesql_core::{Db, QueryOutput, Value};
use tempfile::TempDir;

fn seeded(values: &[&str]) -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("collate.esql")).unwrap();
    db.query("CREATE TABLE c (n text, k int64)").unwrap();
    for (i, v) in values.iter().enumerate() {
        db.query_params(
            "INSERT INTO c (n, k) VALUES (?, ?)",
            &[Value::Text((*v).into()), Value::Int64(i as i64)],
        )
        .unwrap();
    }
    (dir, db)
}

fn names(db: &Db, sql: &str) -> Vec<String> {
    let QueryOutput::Rows { rows, .. } = db.query(sql).unwrap() else {
        panic!("expected rows from {sql}");
    };
    rows.into_iter()
        .map(|row| match row.into_iter().next() {
            Some(Value::Text(s)) => s,
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn order_by_text_is_alphabetical_by_default() {
    let (_d, db) = seeded(&["Zebra", "arbol", "Ávila", "ñu", "acción", "nube", "Ñandú"]);
    assert_eq!(
        names(&db, "SELECT n FROM c ORDER BY n"),
        ["acción", "arbol", "Ávila", "nube", "Ñandú", "ñu", "Zebra"]
    );
}

/// The old behavior is still reachable, and is still the wrong answer for text.
#[test]
fn collate_binary_restores_byte_order() {
    let (_d, db) = seeded(&["Zebra", "arbol", "Ávila", "ñu"]);
    assert_eq!(
        names(&db, "SELECT n FROM c ORDER BY n COLLATE binary"),
        ["Zebra", "arbol", "Ávila", "ñu"]
    );
}

#[test]
fn descending_reverses_the_collated_order() {
    let (_d, db) = seeded(&["arbol", "Ávila", "ñu", "nube"]);
    assert_eq!(
        names(&db, "SELECT n FROM c ORDER BY n DESC"),
        ["ñu", "nube", "Ávila", "arbol"]
    );
}

/// COLLATE is accepted on either side of ASC/DESC, and quoted as MySQL writes
/// it, so word order is not a parse error.
#[test]
fn collate_syntax_variants_agree() {
    let (_d, db) = seeded(&["Zebra", "arbol", "Ávila"]);
    let expected = names(&db, "SELECT n FROM c ORDER BY n COLLATE binary DESC");
    for sql in [
        "SELECT n FROM c ORDER BY n DESC COLLATE binary",
        "SELECT n FROM c ORDER BY n DESC COLLATE 'binary'",
        "SELECT n FROM c ORDER BY n COLLATE BINARY DESC",
    ] {
        assert_eq!(names(&db, sql), expected, "for {sql}");
    }
}

#[test]
fn an_unknown_collation_names_the_ones_that_exist() {
    let (_d, db) = seeded(&["a"]);
    let err = db
        .query("SELECT n FROM c ORDER BY n COLLATE es_ES")
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown collation 'es_ES'"), "{err}");
    assert!(err.contains("unicode"), "{err}");
    assert!(err.contains("binary"), "{err}");
}

/// Per-key collation: one ORDER BY can mix both.
#[test]
fn each_key_carries_its_own_collation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("mixed.esql")).unwrap();
    db.query("CREATE TABLE t (a text, b text)").unwrap();
    db.query("INSERT INTO t (a, b) VALUES ('x', 'Zebra'), ('x', 'arbol'), ('w', 'Ávila')")
        .unwrap();
    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT a, b FROM t ORDER BY a, b COLLATE binary")
        .unwrap()
    else {
        panic!("expected rows");
    };
    let pairs: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Text(a), Value::Text(b)) => (a.clone(), b.clone()),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    // 'w' first by the collated key; within 'x', byte order puts Zebra first.
    assert_eq!(
        pairs,
        [
            ("w".to_string(), "Ávila".to_string()),
            ("x".to_string(), "Zebra".to_string()),
            ("x".to_string(), "arbol".to_string()),
        ]
    );
}

/// Aggregate ORDER BY addresses output columns and must collate them too.
#[test]
fn aggregate_output_ordering_collates() {
    let (_d, db) = seeded(&["Zebra", "arbol", "Ávila", "ñu", "nube"]);
    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT n, count(*) FROM c GROUP BY n ORDER BY n")
        .unwrap()
    else {
        panic!("expected rows");
    };
    let ordered: Vec<String> = rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    assert_eq!(ordered, ["arbol", "Ávila", "nube", "ñu", "Zebra"]);
}

/// The sort spills to disk over the query budget and merges runs; the merge
/// comparator has to collate exactly like the in-memory one, or the output
/// comes back subtly out of order only on large inputs.
#[test]
fn collation_survives_spilling_to_disk() {
    use elitesql_core::{DbOptions, MemoryOptions};
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_or_create_with(
        dir.path().join("spill.esql"),
        DbOptions {
            memory: MemoryOptions {
                // Small enough that a few thousand rows cannot be sorted in one
                // buffer, forcing the external merge path.
                query_working_bytes: 64 * 1024,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE big (n text)").unwrap();

    let alphabet = ["acción", "arbol", "Ávila", "nube", "Ñandú", "ñu", "Zebra"];
    let mut expected = Vec::new();
    for round in 0..500 {
        for word in alphabet {
            let value = format!("{word}{round:04}");
            db.query_params(
                "INSERT INTO big (n) VALUES (?)",
                &[Value::Text(value.clone())],
            )
            .unwrap();
            expected.push(value);
        }
    }
    expected.sort_by(|a, b| elitesql_core::Collation::Unicode.compare(a, b));

    let got = names(&db, "SELECT n FROM big ORDER BY n");
    assert_eq!(got.len(), expected.len());
    assert_eq!(got, expected, "spilled merge must collate like the buffer");
}
