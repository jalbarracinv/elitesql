//! Phase 3 acceptance: a crash during async vector indexing never corrupts
//! canonical data. Same harness pattern as crash_kill.rs — a worker process
//! commits records with vectors in Async mode and is SIGKILLed mid-stream;
//! the parent reopens, checks every acked record (and its vector payload)
//! survived, and that the rebuilt ANN index searches cleanly.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use elitesql_core::{
    check, Column, ColumnType, Db, DbOptions, Durability, IndexingMode, Record, TableSchema, Value,
    VectorIndexOptions, VectorSearchOptions,
};

const ENV_WORKER: &str = "ELITESQL_VECTOR_CRASH_DIR";
const DIM: usize = 8;

fn worker_opts() -> DbOptions {
    DbOptions {
        durability: Durability::Safe,
        memtable_max_bytes: 32 * 1024,
        ..DbOptions::default()
    }
}

fn seq_vector(seq: i64) -> Vec<f32> {
    (0..DIM)
        .map(|j| ((seq * 31 + j as i64) % 97) as f32 / 97.0)
        .collect()
}

#[test]
fn vector_crash_worker() {
    let Ok(dir) = std::env::var(ENV_WORKER) else {
        return;
    };
    let db = Db::open_or_create_with(&dir, worker_opts()).unwrap();
    if !db.tables().contains(&"docs".to_string()) {
        db.create_table(TableSchema::new(
            "docs",
            vec![
                Column::new("n", ColumnType::Int64).not_null(),
                Column::vector("embedding", DIM),
            ],
        ))
        .unwrap();
        db.create_vector_index(
            "docs",
            "embedding",
            VectorIndexOptions {
                mode: IndexingMode::Async,
                ..Default::default()
            },
        )
        .unwrap();
    }

    let ack_path = Path::new(&dir).join("ack.log");
    let mut seq: i64 = std::fs::read_to_string(&ack_path)
        .ok()
        .and_then(|s| s.lines().filter_map(|l| l.trim().parse::<i64>().ok()).max())
        .unwrap_or(0);
    let mut ack = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ack_path)
        .unwrap();

    loop {
        seq += 1;
        let mut rec = Record::new();
        rec.insert("id".into(), Value::Text(format!("v-{seq:08}")));
        rec.insert("n".into(), Value::Int64(seq));
        rec.insert("embedding".into(), Value::Vector(seq_vector(seq)));
        db.insert("docs", rec).unwrap();
        ack.write_all(format!("{seq}\n").as_bytes()).unwrap();
        ack.sync_data().unwrap();
    }
}

#[test]
fn kill9_during_async_indexing_preserves_canonical_data() {
    if std::env::var(ENV_WORKER).is_ok() {
        return;
    }
    let iterations: u32 = std::env::var("ELITESQL_CRASH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("veccrash.esql");
    let exe = std::env::current_exe().unwrap();
    let mut rng: u64 = 0xFEED_FACE_CAFE_BEEF;

    for round in 0..iterations {
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "vector_crash_worker", "--nocapture"])
            .env(ENV_WORKER, db_dir.as_os_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(25 + (rng % 150)));
        child.kill().unwrap();
        child.wait().unwrap();

        let db = Db::open_with(&db_dir, worker_opts())
            .unwrap_or_else(|e| panic!("round {round}: recovery failed: {e}"));

        let acked: Vec<i64> = std::fs::read_to_string(db_dir.join("ack.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();

        // Every acked record AND its exact vector payload survived: async
        // indexing must never put canonical data at risk.
        for seq in &acked {
            let rec = db
                .get("docs", &format!("v-{seq:08}"))
                .unwrap()
                .unwrap_or_else(|| panic!("round {round}: acked record {seq} lost"));
            assert_eq!(
                rec["embedding"],
                Value::Vector(seq_vector(*seq)),
                "round {round}: vector payload of {seq} corrupted"
            );
        }

        // The rebuilt index is immediately usable.
        if !acked.is_empty() {
            let hits = db
                .search_vector(
                    "docs",
                    "embedding",
                    &seq_vector(acked[acked.len() / 2]),
                    5.min(acked.len()),
                    &VectorSearchOptions::default(),
                )
                .unwrap();
            assert!(
                !hits.is_empty(),
                "round {round}: rebuilt index returned nothing"
            );
        }
        drop(db);
    }

    let report = check(&db_dir).unwrap();
    assert!(report.is_ok(), "post-crash check: {:?}", report.errors);
}
