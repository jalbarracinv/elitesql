//! Deterministic corruption fuzzing: flip random bytes in the manifest, WAL
//! and segment files, then open. The engine must never panic and must never
//! accept invalid state: every open either succeeds with a readable database
//! or fails with a clean error.

use std::path::Path;

use clawdb_core::{Column, ColumnType, Db, Record, TableSchema, Value};

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

fn build_template(path: &Path) {
    let db = Db::create(path).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
        ],
    ))
    .unwrap();
    for i in 0..40 {
        let mut r = Record::new();
        r.insert("title".into(), Value::Text(format!("doc {i}")));
        r.insert("score".into(), Value::Int64(i));
        db.insert("docs", r).unwrap();
    }
    db.checkpoint().unwrap();
    // Leave a WAL tail too, so both paths get fuzzed.
    for i in 40..60 {
        let mut r = Record::new();
        r.insert("title".into(), Value::Text(format!("doc {i}")));
        r.insert("score".into(), Value::Int64(i));
        db.insert("docs", r).unwrap();
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

fn candidate_files(db_path: &Path) -> Vec<std::path::PathBuf> {
    let mut files = vec![db_path.join("manifest")];
    for sub in ["wal", "segments"] {
        if let Ok(entries) = std::fs::read_dir(db_path.join(sub)) {
            for e in entries.flatten() {
                files.push(e.path());
            }
        }
    }
    files.retain(|f| f.is_file() && std::fs::metadata(f).map(|m| m.len() > 0).unwrap_or(false));
    files
}

#[test]
fn random_byte_flips_never_panic_or_corrupt_silently() {
    let template_dir = tempfile::tempdir().unwrap();
    let template = template_dir.path().join("template.clawdb");
    build_template(&template);

    let iterations: u64 = std::env::var("CLAWDB_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    for seed in 0..iterations {
        let mut rng = XorShift(seed * 2_654_435_761 + 1);
        let work_dir = tempfile::tempdir().unwrap();
        let db_path = work_dir.path().join("fuzzed.clawdb");
        copy_dir(&template, &db_path);
        // The template's LOCK file is advisory only; a fresh open re-locks it.

        let files = candidate_files(&db_path);
        let target = &files[rng.below(files.len() as u64) as usize];
        let mut bytes = std::fs::read(target).unwrap();
        let flips = 1 + rng.below(3);
        for _ in 0..flips {
            let pos = rng.below(bytes.len() as u64) as usize;
            bytes[pos] ^= (1 + rng.below(255)) as u8;
        }
        std::fs::write(target, &bytes).unwrap();

        // Must not panic. Ok => the surviving state must be fully readable.
        // A clean Err refusal is also a valid outcome.
        if let Ok(db) = Db::open(&db_path) {
            let rows = db.scan("docs").expect("open db must be readable");
            for (_, rec) in &rows {
                assert!(matches!(rec.get("title"), Some(Value::Text(_))));
            }
        }
    }
}
