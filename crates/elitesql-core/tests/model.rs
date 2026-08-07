//! Property-style model test: random operation sequences run against both
//! the engine and an in-memory model; states must match at every checkpoint,
//! across snapshots, transactions, reopens and compaction.

use std::collections::BTreeMap;

use elitesql_core::{
    Column, ColumnType, Db, DbOptions, Durability, Record, Snapshot, TableSchema, Value,
};

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

type Model = BTreeMap<String, i64>;

fn opts() -> DbOptions {
    DbOptions {
        durability: Durability::Fast,
        // Small threshold so the run crosses several real checkpoints.
        memtable_max_bytes: 16 * 1024,
        ..DbOptions::default()
    }
}

fn open_db(path: &std::path::Path) -> Db {
    Db::open_or_create_with(path, opts()).unwrap()
}

fn record(v: i64) -> Record {
    let mut r = Record::new();
    r.insert("v".into(), Value::Int64(v));
    r
}

fn assert_matches_model(db: &Db, model: &Model, ctx: &str) {
    let rows = db.scan("m").unwrap();
    assert_eq!(rows.len(), model.len(), "row count mismatch ({ctx})");
    for (id, rec) in rows {
        let expected = model.get(&id).unwrap_or_else(|| panic!("unexpected id {id} ({ctx})"));
        assert_eq!(rec["v"], Value::Int64(*expected), "value mismatch for {id} ({ctx})");
    }
}

fn assert_matches_snapshot(db: &Db, snap: &Snapshot, model: &Model, ctx: &str) {
    let rows = db.scan_at(snap, "m").unwrap();
    assert_eq!(rows.len(), model.len(), "snapshot row count mismatch ({ctx})");
    for (id, rec) in rows {
        let expected = model.get(&id).unwrap();
        assert_eq!(rec["v"], Value::Int64(*expected), "snapshot mismatch for {id} ({ctx})");
    }
}

#[test]
fn engine_matches_model_under_random_workload() {
    for seed in 1..=3u64 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.esql");
        {
            let db = open_db(&path);
            db.create_table(TableSchema::new(
                "m",
                vec![Column::new("v", ColumnType::Int64).not_null()],
            ))
            .unwrap();
        }

        let mut rng = XorShift(seed * 0x9E37_79B9 + 7);
        let mut model: Model = BTreeMap::new();
        let mut db = open_db(&path);
        let mut snapshots: Vec<(Snapshot, Model)> = Vec::new();
        let mut counter = 0i64;

        for step in 0..500 {
            match rng.below(100) {
                // insert
                0..=34 => {
                    counter += 1;
                    let id = db.insert("m", record(counter)).unwrap();
                    model.insert(id, counter);
                }
                // update existing
                35..=59 => {
                    if let Some(id) = pick_key(&model, &mut rng) {
                        counter += 1;
                        let mut p = Record::new();
                        p.insert("v".into(), Value::Int64(counter));
                        db.update("m", &id, p).unwrap();
                        model.insert(id, counter);
                    }
                }
                // delete existing
                60..=74 => {
                    if let Some(id) = pick_key(&model, &mut rng) {
                        assert!(db.delete("m", &id).unwrap());
                        model.remove(&id);
                    }
                }
                // multi-op transaction (all-or-nothing, applied to model on commit)
                75..=84 => {
                    let mut txn = db.begin();
                    let mut staged = Vec::new();
                    for _ in 0..3 {
                        counter += 1;
                        let id = txn.insert("m", record(counter)).unwrap();
                        staged.push((id, counter));
                    }
                    if rng.below(2) == 0 {
                        txn.commit().unwrap();
                        for (id, v) in staged {
                            model.insert(id, v);
                        }
                    } else {
                        txn.rollback(); // model untouched
                    }
                }
                // point-read comparison
                85..=92 => {
                    if let Some(id) = pick_key(&model, &mut rng) {
                        let rec = db.get("m", &id).unwrap().unwrap();
                        assert_eq!(rec["v"], Value::Int64(model[&id]));
                    }
                    // A missing id reads as None.
                    assert!(db.get("m", "never-created").unwrap().is_none());
                }
                // take a snapshot to verify later
                93..=95 => {
                    if snapshots.len() < 3 {
                        snapshots.push((db.snapshot(), model.clone()));
                    }
                }
                // compact (respecting snapshots), occasionally
                96 => {
                    db.compact().unwrap();
                }
                // reopen: snapshots don't outlive the handle
                _ => {
                    snapshots.clear();
                    drop(db);
                    db = open_db(&path);
                    assert_matches_model(&db, &model, &format!("seed {seed} step {step} reopen"));
                }
            }

            if step % 50 == 49 {
                assert_matches_model(&db, &model, &format!("seed {seed} step {step}"));
                for (i, (snap, snap_model)) in snapshots.iter().enumerate() {
                    assert_matches_snapshot(
                        &db,
                        snap,
                        snap_model,
                        &format!("seed {seed} step {step} snap {i}"),
                    );
                }
            }
        }

        // Final: full verification, then compaction, then reopen.
        assert_matches_model(&db, &model, &format!("seed {seed} final"));
        for (i, (snap, snap_model)) in snapshots.iter().enumerate() {
            assert_matches_snapshot(&db, snap, snap_model, &format!("seed {seed} final snap {i}"));
        }
        snapshots.clear();
        db.compact().unwrap();
        assert_matches_model(&db, &model, &format!("seed {seed} post-compact"));
        drop(db);
        let db = open_db(&path);
        assert_matches_model(&db, &model, &format!("seed {seed} post-reopen"));
    }
}

fn pick_key(model: &Model, rng: &mut XorShift) -> Option<String> {
    if model.is_empty() {
        return None;
    }
    let n = rng.below(model.len() as u64) as usize;
    model.keys().nth(n).cloned()
}
