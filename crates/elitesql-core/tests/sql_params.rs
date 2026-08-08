use std::collections::BTreeMap;

use elitesql_core::{Db, Error, QueryOutput, Record, Value};
use serde_json::json;

fn rows(output: QueryOutput) -> Vec<Vec<Value>> {
    match output {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn positional_parameters_preserve_types_and_cannot_inject_sql() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("params.esql")).unwrap();
    db.query(
        "CREATE TABLE samples (name text NOT NULL, enabled bool NOT NULL, score float64 NOT NULL, payload blob NOT NULL, happened timestamp NOT NULL, day date NOT NULL, clock time NOT NULL, metadata json NOT NULL, embedding vector(3) NOT NULL)",
    )
    .unwrap();

    let hostile = "alice' OR TRUE --";
    let metadata = json!({"nested": [1, true, "x"], "percent": "%s"});
    let params = vec![
        Value::Text(hostile.into()),
        Value::Bool(true),
        Value::Float64(2.5),
        Value::Blob(vec![0, 1, 255]),
        Value::Timestamp(1_787_702_400_123_456),
        Value::Date(20_674),
        Value::Time(45_296_000_007),
        Value::Json(metadata.clone()),
        Value::Vector(vec![0.25, -1.0, 3.5]),
    ];
    db.query_params(
        "INSERT INTO samples (name, enabled, score, payload, happened, day, clock, metadata, embedding) VALUES (%s, ?, %s, ?, %s, ?, %s, ?, %s)",
        &params,
    )
    .unwrap();

    let stored = db.scan("samples").unwrap();
    assert_eq!(stored.len(), 1);
    let record = &stored[0].1;
    assert_eq!(record["name"], Value::Text(hostile.into()));
    assert_eq!(record["payload"], Value::Blob(vec![0, 1, 255]));
    assert_eq!(record["metadata"], Value::Json(metadata));
    assert_eq!(record["embedding"], Value::Vector(vec![0.25, -1.0, 3.5]));

    // The quote/comment payload is data, never executable SQL.
    let selected = rows(
        db.query_params(
            "SELECT name FROM samples WHERE name = %s LIMIT %s OFFSET ?",
            &[
                Value::Text(hostile.into()),
                Value::Int64(1),
                Value::Int64(0),
            ],
        )
        .unwrap(),
    );
    assert_eq!(selected, vec![vec![Value::Text(hostile.into())]]);
}

#[test]
fn named_parameters_can_repeat_and_work_in_predicates_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("named.esql")).unwrap();
    db.query("CREATE TABLE numbers (n int64 NOT NULL, label text NOT NULL)")
        .unwrap();
    for n in 0..8 {
        db.query_params(
            "INSERT INTO numbers (id, n, label) VALUES (%s, %s, %s)",
            &[
                Value::Text(format!("n-{n}")),
                Value::Int64(n),
                Value::Text(format!("value-{n}")),
            ],
        )
        .unwrap();
    }

    let mut params = Record::new();
    params.insert("floor".into(), Value::Int64(2));
    params.insert("ceiling".into(), Value::Int64(6));
    params.insert("limit".into(), Value::Int64(3));
    let selected = rows(
        db.query_named_params(
            "SELECT n FROM numbers WHERE n >= %(floor)s AND n <= %(ceiling)s AND n != %(floor)s ORDER BY n LIMIT %(limit)s",
            &params,
        )
        .unwrap(),
    );
    assert_eq!(
        selected,
        vec![
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)]
        ]
    );

    let mut in_params = Record::new();
    in_params.insert("a".into(), Value::Int64(1));
    in_params.insert("b".into(), Value::Int64(7));
    assert_eq!(
        rows(
            db.query_named_params(
                "SELECT n FROM numbers WHERE n IN (%(a)s, %(b)s) ORDER BY n",
                &in_params,
            )
            .unwrap()
        ),
        vec![vec![Value::Int64(1)], vec![Value::Int64(7)]]
    );
}

#[test]
fn parameter_shape_and_count_are_strictly_validated() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("errors.esql")).unwrap();
    db.query("CREATE TABLE items (name text NOT NULL)").unwrap();

    assert!(matches!(
        db.query("SELECT * FROM items WHERE name = %s"),
        Err(Error::InvalidArgument(message)) if message.contains("missing positional")
    ));
    assert!(matches!(
        db.query_params("SELECT * FROM items", &[Value::Text("unused".into())]),
        Err(Error::InvalidArgument(message)) if message.contains("unused positional")
    ));
    assert!(matches!(
        db.query_params(
            "SELECT * FROM items WHERE name = %s OR name = ?",
            &[Value::Text("one".into())]
        ),
        Err(Error::InvalidArgument(message)) if message.contains("parameter 2")
    ));

    let mut named = BTreeMap::new();
    named.insert("wrong".into(), Value::Text("x".into()));
    assert!(matches!(
        db.query_named_params("SELECT * FROM items WHERE name = %(wanted)s", &named),
        Err(Error::InvalidArgument(message)) if message.contains("wanted")
    ));
    assert!(matches!(
        db.query_named_params("SELECT * FROM items", &named),
        Err(Error::InvalidArgument(message)) if message.contains("unused named")
    ));
    assert!(matches!(
        db.query_params("SELECT * FROM items LIMIT %s", &[Value::Text("10".into())]),
        Err(Error::InvalidArgument(message)) if message.contains("LIMIT/OFFSET")
    ));
    assert!(matches!(
        db.query_params("SELECT * FROM items LIMIT ?", &[Value::Int64(-1)]),
        Err(Error::InvalidArgument(message)) if message.contains("non-negative")
    ));

    // Placeholder-looking text inside strings and comments is not bound.
    assert!(db
        .query("SELECT * FROM items WHERE name = '%s' -- ? %(ignored)s")
        .is_ok());
}

#[test]
fn streaming_cursor_accepts_typed_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("cursor-params.esql")).unwrap();
    db.query("CREATE TABLE items (n int64 NOT NULL)").unwrap();
    for n in 0..10 {
        db.query_params(
            "INSERT INTO items (id, n) VALUES (?, ?)",
            &[Value::Text(format!("i-{n:02}")), Value::Int64(n)],
        )
        .unwrap();
    }
    let mut cursor = db
        .query_cursor_params(
            "SELECT n FROM items WHERE n >= ? LIMIT ? OFFSET ?",
            &[Value::Int64(4), Value::Int64(3), Value::Int64(1)],
        )
        .unwrap();
    assert_eq!(
        cursor.next_batch(10).unwrap(),
        vec![
            vec![Value::Int64(5)],
            vec![Value::Int64(6)],
            vec![Value::Int64(7)]
        ]
    );
}

#[test]
fn update_delete_and_ddl_defaults_bind_without_text_substitution() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("write-params.esql")).unwrap();
    db.query_params(
        "CREATE TABLE docs (name text NOT NULL DEFAULT %s, score int64)",
        &[Value::Text("default's value".into())],
    )
    .unwrap();
    db.query_params(
        "INSERT INTO docs (id, score) VALUES (?, ?)",
        &[Value::Text("doc-1".into()), Value::Int64(1)],
    )
    .unwrap();
    db.query_params(
        "UPDATE docs SET name = %s, score = ? WHERE id = %s",
        &[
            Value::Text("updated' safely".into()),
            Value::Int64(9),
            Value::Text("doc-1".into()),
        ],
    )
    .unwrap();
    assert_eq!(
        db.get("docs", "doc-1").unwrap().unwrap()["name"],
        Value::Text("updated' safely".into())
    );
    db.query_params("DELETE FROM docs WHERE score = ?", &[Value::Int64(9)])
        .unwrap();
    assert!(db.get("docs", "doc-1").unwrap().is_none());
}

#[test]
fn json_transport_accepts_tagged_int64_without_losing_precision() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("json-params.esql")).unwrap();
    db.query("CREATE TABLE numbers (n int64 NOT NULL)").unwrap();
    elitesql_core::jsonio::query_with_params_json(
        &db,
        "INSERT INTO numbers (n) VALUES (%s)",
        &json!([{"$t": "int64", "v": "9007199254740993"}]),
    )
    .unwrap();
    assert_eq!(
        db.scan("numbers").unwrap()[0].1["n"],
        Value::Int64(9_007_199_254_740_993)
    );
}
