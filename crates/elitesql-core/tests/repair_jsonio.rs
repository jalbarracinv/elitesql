//! Phase 4: salvage (repair) and the JSON marshaling used by bindings.

use elitesql_core::{jsonio, salvage, Db, Record, Value};

#[test]
fn jsonio_value_roundtrip() {
    let values = vec![
        Value::Null,
        Value::Bool(true),
        Value::Int64(-42),
        Value::Float64(2.5),
        Value::Text("hola".into()),
        Value::Blob(vec![0xDE, 0xAD, 0x00, 0xEF]),
        Value::Timestamp(1_722_000_000_000_000),
        Value::Json(serde_json::json!({"k": [1, 2, {"n": null}]})),
        Value::Vector(vec![0.5, -1.25]),
        Value::parse_date("2026-08-07").unwrap(),
        Value::parse_time("09:30:00.250000").unwrap(),
    ];
    for v in &values {
        let j = jsonio::value_to_json(v);
        let back = jsonio::json_to_value(&j).unwrap();
        assert_eq!(&back, v, "roundtrip failed for {v:?} via {j}");
    }
    // Formatting helpers.
    assert_eq!(jsonio::format_date(0), "1970-01-01");
    assert_eq!(jsonio::format_time(34_200_000_000), "09:30:00");
    assert_eq!(
        jsonio::format_timestamp(
            match Value::parse_timestamp("2026-08-07 09:30:00").unwrap() {
                Value::Timestamp(us) => us,
                _ => unreachable!(),
            }
        ),
        "2026-08-07 09:30:00Z"
    );
}

#[test]
fn salvage_recovers_valid_prefix_and_reports_damage() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("damaged.esql");
    let dst = dir.path().join("salvaged.esql");

    {
        let db = Db::create(&src).unwrap();
        db.query("CREATE TABLE docs (title text NOT NULL, score int64)")
            .unwrap();
        for i in 0..40 {
            db.query(&format!(
                "INSERT INTO docs (id, title, score) VALUES ('d-{i:03}', 'doc {i}', {i})"
            ))
            .unwrap();
        }
        db.query("DELETE FROM docs WHERE id = 'd-000'").unwrap();
        db.query("UPDATE docs SET score = 999 WHERE id = 'd-001'")
            .unwrap();
        db.checkpoint().unwrap(); // everything into a segment
        for i in 40..50 {
            db.query(&format!(
                "INSERT INTO docs (id, title, score) VALUES ('d-{i:03}', 'doc {i}', {i})"
            ))
            .unwrap(); // these live in the WAL
        }
    }

    // Corrupt the middle of the segment: entries after the flip are lost.
    let seg = std::fs::read_dir(src.join("segments"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "seg"))
        .unwrap();
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&seg, &bytes).unwrap();

    // A normal open refuses (corrupt segment); salvage recovers the valid
    // prefix. Entries after the corruption point (which may include newer
    // versions, tombstones or updates) are lost — and REPORTED.
    assert!(
        Db::open(&src).is_err(),
        "corrupt segment must fail normal open"
    );
    let report = salvage(&src, &dst).unwrap();
    assert_eq!(report.tables, vec!["docs".to_string()]);
    assert!(
        report.recovered_records > 20,
        "prefix + WAL should recover: {report:?}"
    );
    assert!(
        report.notes.iter().any(|n| n.contains("discarded")),
        "damage must be reported, never silent: {:?}",
        report.notes
    );

    // The salvaged database opens, matches the report, and validates.
    let db = Db::open(&dst).unwrap();
    let rows = db.scan("docs").unwrap();
    assert_eq!(rows.len() as u64, report.recovered_records);
    // WAL-tail records survived even though the segment was damaged.
    assert!(db.get("docs", "d-045").unwrap().is_some());
    drop(db);
    let check = elitesql_core::check(&dst).unwrap();
    assert!(
        check.is_ok(),
        "salvaged db must validate: {:?}",
        check.errors
    );
}

#[test]
fn salvage_preserves_updates_and_deletes_when_segments_are_intact() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("wal-damaged.esql");
    let dst = dir.path().join("salvaged2.esql");

    {
        let db = Db::create(&src).unwrap();
        db.query("CREATE TABLE docs (title text NOT NULL, score int64)")
            .unwrap();
        for i in 0..20 {
            db.query(&format!(
                "INSERT INTO docs (id, title, score) VALUES ('d-{i:03}', 'doc {i}', {i})"
            ))
            .unwrap();
        }
        db.query("DELETE FROM docs WHERE id = 'd-000'").unwrap();
        db.query("UPDATE docs SET score = 999 WHERE id = 'd-001'")
            .unwrap();
        db.checkpoint().unwrap(); // segment now holds inserts + tombstone + update
        for i in 20..30 {
            db.query(&format!(
                "INSERT INTO docs (id, title, score) VALUES ('d-{i:03}', 'doc {i}', {i})"
            ))
            .unwrap(); // WAL only
        }
    }
    // Damage the WAL in the middle: its tail commits are lost, segment intact.
    let wal = std::fs::read_dir(src.join("wal"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .unwrap();
    let mut bytes = std::fs::read(&wal).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&wal, &bytes).unwrap();

    let report = salvage(&src, &dst).unwrap();
    let db = Db::open(&dst).unwrap();
    // Semantics from the intact segment hold exactly:
    assert!(
        db.get("docs", "d-000").unwrap().is_none(),
        "tombstone respected"
    );
    assert_eq!(
        db.get("docs", "d-001").unwrap().unwrap()["score"],
        Value::Int64(999),
        "update preserved"
    );
    assert_eq!(report.deleted_records, 1);
    // A prefix of the WAL-only records survived; the damage was reported.
    assert!(
        db.get("docs", "d-020").unwrap().is_some(),
        "wal prefix survives"
    );
    assert!(
        report.notes.iter().any(|n| n.contains("torn tail")),
        "wal damage must be reported: {:?}",
        report.notes
    );
}

#[test]
fn salvage_refuses_existing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.esql");
    {
        let db = Db::create(&src).unwrap();
        db.query("CREATE TABLE t (a int64)").unwrap();
    }
    let dst = dir.path().join("dst.esql");
    std::fs::create_dir_all(&dst).unwrap();
    assert!(salvage(&src, &dst).is_err());
}

#[test]
fn import_export_type_mapping() {
    // json_to_value_for_type accepts natural encodings per column type.
    use elitesql_core::ColumnType as T;
    let cases = vec![
        (serde_json::json!(5), T::Int64, Value::Int64(5)),
        (serde_json::json!(2.5), T::Float64, Value::Float64(2.5)),
        (
            serde_json::json!("hola"),
            T::Text,
            Value::Text("hola".into()),
        ),
        (
            serde_json::json!("2026-08-07"),
            T::Date,
            Value::parse_date("2026-08-07").unwrap(),
        ),
        (
            serde_json::json!("09:30:00"),
            T::Time,
            Value::parse_time("09:30:00").unwrap(),
        ),
        (
            serde_json::json!("2026-08-07 09:30:00"),
            T::Timestamp,
            Value::parse_timestamp("2026-08-07 09:30:00").unwrap(),
        ),
        (
            serde_json::json!([1.0, 2.0]),
            T::Vector,
            Value::Vector(vec![1.0, 2.0]),
        ),
        (
            serde_json::json!("00ff"),
            T::Blob,
            Value::Blob(vec![0, 255]),
        ),
        (
            serde_json::json!({"a": 1}),
            T::Json,
            Value::Json(serde_json::json!({"a": 1})),
        ),
    ];
    for (j, ty, expected) in cases {
        let got = jsonio::json_to_value_for_type(&j, ty).unwrap();
        assert_eq!(got, expected, "for {j} as {ty}");
    }
    assert!(jsonio::json_to_value_for_type(&serde_json::json!("nope"), T::Int64).is_err());

    let mut rec = Record::new();
    rec.insert("id".into(), Value::Text("r1".into()));
    rec.insert("day".into(), Value::parse_date("2026-08-07").unwrap());
    let j = jsonio::record_to_json(&rec);
    assert_eq!(j["id"], serde_json::json!("r1"));
    assert_eq!(j["day"]["$t"], serde_json::json!("date"));
    assert_eq!(j["day"]["iso"], serde_json::json!("2026-08-07"));
}
