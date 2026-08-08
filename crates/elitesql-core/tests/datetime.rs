//! Phase 2.5: date (days since epoch) and time (microseconds since midnight)
//! types — API + SQL roundtrip, literal validation, comparisons, indexes,
//! ORDER BY and aggregate interaction.

use elitesql_core::{Db, Error, QueryOutput, Record, Value};
use tempfile::TempDir;

fn new_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("dt.esql")).unwrap();
    db.query("CREATE TABLE events (name text NOT NULL, day date, at time)")
        .unwrap();
    (dir, db)
}

fn rows(out: QueryOutput) -> Vec<Vec<Value>> {
    match out {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn value_constructors_and_parsing() {
    // 1970-01-01 is day 0; 1970-01-02 is day 1.
    assert_eq!(Value::date_from_ymd(1970, 1, 1), Some(Value::Date(0)));
    assert_eq!(Value::date_from_ymd(1970, 1, 2), Some(Value::Date(1)));
    assert_eq!(Value::parse_date("1969-12-31"), Some(Value::Date(-1)));
    // 2000-03-01: leap year handling around Feb 29.
    assert_eq!(Value::parse_date("2000-02-29"), Some(Value::Date(11016)));
    assert_eq!(Value::parse_date("2000-03-01"), Some(Value::Date(11017)));
    // Invalid dates.
    assert_eq!(Value::parse_date("2026-02-30"), None);
    assert_eq!(Value::parse_date("2026-13-01"), None);
    assert_eq!(
        Value::parse_date("2100-02-29"),
        None,
        "2100 is not a leap year"
    );
    assert_eq!(Value::parse_date("not-a-date"), None);
    assert_eq!(Value::parse_date("2026-08"), None);

    assert_eq!(Value::time_from_hms_micro(0, 0, 0, 0), Some(Value::Time(0)));
    assert_eq!(
        Value::parse_time("23:59:59.999999"),
        Some(Value::Time(86_399_999_999))
    );
    assert_eq!(
        Value::parse_time("09:30:00"),
        Some(Value::Time(34_200_000_000))
    );
    assert_eq!(
        Value::parse_time("09:30:00.5"),
        Some(Value::Time(34_200_500_000))
    );
    assert_eq!(Value::parse_time("25:00:00"), None);
    assert_eq!(Value::parse_time("09:60:00"), None);
    assert_eq!(Value::parse_time("09:30"), None, "seconds are required");
}

#[test]
fn timestamp_accepts_datetime_literals() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("ts.esql")).unwrap();
    db.query("CREATE TABLE logs (msg text NOT NULL, at timestamp)")
        .unwrap();
    db.query(
        "INSERT INTO logs (msg, at) VALUES \
         ('a', '2026-08-07 09:30:00'), \
         ('b', '2026-08-07T10:00:00Z'), \
         ('c', '2026-08-07 10:00:00.250000'), \
         ('d', '2026-08-08'), \
         ('e', 1722000000000000)",
    )
    .unwrap();

    // Parsing agrees with the public helper and the raw microsecond value.
    let expected = Value::parse_timestamp("2026-08-07 09:30:00").unwrap();
    let r = rows(db.query("SELECT at FROM logs WHERE msg = 'a'").unwrap());
    assert_eq!(r[0][0], expected);
    // Date-only shorthand = midnight UTC.
    assert_eq!(
        Value::parse_timestamp("2026-08-08"),
        Value::parse_timestamp("2026-08-08 00:00:00")
    );
    // T separator and Z suffix are equivalent to the space form.
    assert_eq!(
        Value::parse_timestamp("2026-08-07T10:00:00Z"),
        Value::parse_timestamp("2026-08-07 10:00:00")
    );

    // String literals coerce in WHERE, with fractions ordering correctly.
    let r = rows(
        db.query(
            "SELECT msg FROM logs WHERE at >= '2026-08-07 10:00:00' AND at < '2026-08-08 00:00:00' ORDER BY at",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("b".into()));
    assert_eq!(r[1][0], Value::Text("c".into()));

    // Indexed equality with a datetime string.
    db.query("CREATE INDEX ON logs (at)").unwrap();
    let r = rows(
        db.query("SELECT msg FROM logs WHERE at = '2026-08-07 09:30:00'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);

    // Invalid literals fail clearly; timezone offsets are not supported.
    let err = db
        .query("INSERT INTO logs (msg, at) VALUES ('bad', '2026-08-07 25:00:00')")
        .unwrap_err();
    assert!(err.to_string().contains("YYYY-MM-DD HH:MM:SS"), "{err}");
    let err = db
        .query("INSERT INTO logs (msg, at) VALUES ('bad', '2026-08-07 09:00:00+05:00')")
        .unwrap_err();
    assert!(err.to_string().contains("timestamp literal"), "{err}");
}

#[test]
fn sql_roundtrip_and_comparisons() {
    let (_d, db) = new_db();
    db.query(
        "INSERT INTO events (name, day, at) VALUES \
         ('kickoff', '2026-08-07', '09:30:00'), \
         ('review',  '2026-08-20', '15:00:00'), \
         ('launch',  '2026-09-01', '09:30:00'), \
         ('tbd',     NULL, NULL)",
    )
    .unwrap();

    // Text literals coerce against date columns in WHERE.
    let r = rows(
        db.query("SELECT name FROM events WHERE day = '2026-08-07'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("kickoff".into()));

    let r = rows(
        db.query("SELECT name FROM events WHERE day > '2026-08-10' ORDER BY day")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("review".into()));
    assert_eq!(r[1][0], Value::Text("launch".into()));

    let r = rows(
        db.query("SELECT name FROM events WHERE at = '09:30:00' ORDER BY day")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);

    let r = rows(
        db.query("SELECT name FROM events WHERE day IS NULL")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("tbd".into()));

    // Stored values come back as Date/Time.
    let r = rows(
        db.query("SELECT day, at FROM events WHERE name = 'kickoff'")
            .unwrap(),
    );
    assert!(matches!(r[0][0], Value::Date(_)));
    assert!(matches!(r[0][1], Value::Time(_)));
}

#[test]
fn api_roundtrip_and_ordering() {
    let (_d, db) = new_db();
    let mut rec = Record::new();
    rec.insert("name".into(), Value::Text("api".into()));
    rec.insert("day".into(), Value::parse_date("2025-12-31").unwrap());
    rec.insert("at".into(), Value::time_from_hms_micro(8, 0, 0, 0).unwrap());
    let id = db.insert("events", rec).unwrap();
    let back = db.get("events", &id).unwrap().unwrap();
    assert_eq!(back["day"], Value::parse_date("2025-12-31").unwrap());
    assert_eq!(back["at"], Value::Time(28_800_000_000));

    db.query("INSERT INTO events (name, day) VALUES ('a', '2026-01-15'), ('b', '2025-06-01')")
        .unwrap();
    let r = rows(
        db.query("SELECT name FROM events WHERE day IS NOT NULL ORDER BY day DESC")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("a".into()));
    assert_eq!(r[2][0], Value::Text("b".into()));
}

#[test]
fn invalid_literals_are_rejected() {
    let (_d, db) = new_db();
    let err = db
        .query("INSERT INTO events (name, day) VALUES ('bad', '2026-02-30')")
        .unwrap_err();
    assert!(err.to_string().contains("YYYY-MM-DD"), "{err}");
    let err = db
        .query("INSERT INTO events (name, at) VALUES ('bad', '25:00:00')")
        .unwrap_err();
    assert!(err.to_string().contains("HH:MM:SS"), "{err}");
    let err = db
        .query("INSERT INTO events (name, at) VALUES ('bad', 99999999999999)")
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
    // Type mismatch through the API.
    let mut rec = Record::new();
    rec.insert("name".into(), Value::Text("bad".into()));
    rec.insert("day".into(), Value::Text("2026-08-07".into()));
    assert!(matches!(
        db.insert("events", rec),
        Err(Error::SchemaViolation(_))
    ));
}

#[test]
fn date_indexes_and_find_eq() {
    let (_d, db) = new_db();
    db.query("CREATE INDEX ON events (day)").unwrap();
    for i in 1..=28 {
        db.query(&format!(
            "INSERT INTO events (name, day) VALUES ('e{i}', '2026-03-{i:02}')"
        ))
        .unwrap();
    }
    db.query("INSERT INTO events (name, day) VALUES ('dup', '2026-03-15')")
        .unwrap();

    // Indexed equality through SQL (text literal coerced to date for lookup).
    let r = rows(
        db.query("SELECT name FROM events WHERE day = '2026-03-15' ORDER BY name")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);

    // find_eq through the API with a real Date value.
    let hits = db
        .find_eq("events", "day", &Value::parse_date("2026-03-15").unwrap())
        .unwrap();
    assert_eq!(hits.len(), 2);

    // Unique index over date works too.
    db.query("CREATE TABLE days (d date)").unwrap();
    db.query("CREATE UNIQUE INDEX ON days (d)").unwrap();
    db.query("INSERT INTO days (d) VALUES ('2026-01-01')")
        .unwrap();
    assert!(matches!(
        db.query("INSERT INTO days (d) VALUES ('2026-01-01')"),
        Err(Error::UniqueViolation { .. })
    ));
}

#[test]
fn dates_group_and_minmax() {
    let (_d, db) = new_db();
    db.query(
        "INSERT INTO events (name, day) VALUES \
         ('a', '2026-05-01'), ('b', '2026-05-01'), ('c', '2026-06-10'), ('d', NULL)",
    )
    .unwrap();
    let r = rows(
        db.query("SELECT day, count(*) AS n FROM events GROUP BY day HAVING count(*) > 1")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::parse_date("2026-05-01").unwrap());
    assert_eq!(r[0][1], Value::Int64(2));

    let r = rows(db.query("SELECT min(day), max(day) FROM events").unwrap());
    assert_eq!(
        r[0][0],
        Value::parse_date("2026-05-01").unwrap(),
        "MIN ignores NULL"
    );
    assert_eq!(r[0][1], Value::parse_date("2026-06-10").unwrap());
}

#[test]
fn survives_reopen_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dt.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE events (name text NOT NULL, day date, at time)")
            .unwrap();
        db.query(
            "INSERT INTO events (id, name, day, at) VALUES ('e1', 'x', '2026-08-07', '12:00:00')",
        )
        .unwrap();
        db.compact().unwrap();
    }
    let db = Db::open(&path).unwrap();
    let r = rows(
        db.query("SELECT day, at FROM events WHERE id = 'e1'")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::parse_date("2026-08-07").unwrap());
    assert_eq!(r[0][1], Value::parse_time("12:00:00").unwrap());
}
