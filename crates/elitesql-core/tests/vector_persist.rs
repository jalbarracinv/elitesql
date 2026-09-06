//! Persistence of the ANN graph in vectors/: fast load on open, incremental
//! catch-up when the dump is older than the committed state, and fallback to
//! a full rebuild when the dump is corrupt or its definition changed.

use std::path::{Path, PathBuf};

use elitesql_core::{
    AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Record, TableSchema, Value,
    VectorIndexOptions, VectorSearchOptions,
};

const DIM: usize = 16;

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
    fn vec(&mut self, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (self.next() % 10_000) as f32 / 10_000.0 - 0.5)
            .collect()
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::vector("embedding", DIM),
        ],
    )
}

fn doc(title: &str, embedding: Vec<f32>) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(title.into()));
    r.insert("embedding".into(), Value::Vector(embedding));
    r
}

fn try_vidx_file(db_path: &Path) -> Option<PathBuf> {
    std::fs::read_dir(db_path.join("vectors"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "vidx"))
}

fn vidx_file(db_path: &Path) -> PathBuf {
    try_vidx_file(db_path).expect("vidx dump present")
}

fn search_ids(db: &Db, q: &[f32], k: usize) -> Vec<String> {
    let opts = VectorSearchOptions {
        ef_search: Some(200),
        ..Default::default()
    };
    db.search_vector("docs", "embedding", q, k, &opts)
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect()
}

#[test]
fn clean_close_dumps_and_open_loads_identically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(11);
    let q = rng.vec(DIM);
    let expected: Vec<String>;
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..400 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
        expected = search_ids(&db, &q, 10);
    } // drop dumps the graph

    let dump = vidx_file(&path);
    assert!(
        std::fs::metadata(&dump).unwrap().len() > 0,
        "dump written on close"
    );

    let db = Db::open(&path).unwrap();
    assert_eq!(
        search_ids(&db, &q, 10),
        expected,
        "loaded graph answers identically"
    );
}

#[test]
fn stale_dump_catches_up_with_newer_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(22);
    let near = vec![1.0; DIM];

    // Session 1: baseline docs, clean close -> dump at version V1.
    let mut ids = Vec::new();
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..100 {
            ids.push(
                db.insert("docs", doc(&format!("old {i}"), rng.vec(DIM)))
                    .unwrap(),
            );
        }
    }
    // Save the V1 dump aside.
    let dump_path = vidx_file(&path);
    let stale_dump = std::fs::read(&dump_path).unwrap();

    // Session 2: mutate heavily, clean close -> fresh dump at V2.
    let winner;
    {
        let db = Db::open(&path).unwrap();
        winner = db.insert("docs", doc("winner", near.clone())).unwrap();
        for id in ids.iter().take(30) {
            db.delete("docs", id).unwrap();
        }
        let mut patch = Record::new();
        patch.insert(
            "embedding".into(),
            Value::Vector(near.iter().map(|x| x * 0.9).collect()),
        );
        db.update("docs", &ids[40], patch).unwrap();
    }
    // Restore the STALE dump: simulates a crash after those commits (WAL
    // and segments are newer than the persisted graph).
    std::fs::write(&dump_path, &stale_dump).unwrap();

    let db = Db::open(&path).unwrap();
    // `winner` and the updated `ids[40]` are both exactly parallel to the
    // query, so their cosine distances tie at 0 and the order between them is
    // decided by id, not by relevance. Check membership, not position.
    let top = search_ids(&db, &near, 2);
    assert!(
        top.contains(&winner),
        "record committed after the dump is searchable: {top:?}"
    );
    assert!(
        top.contains(&ids[40]),
        "update committed after the dump is reflected: {top:?}"
    );
    // Deletions after the dump never resurface.
    let all = search_ids(&db, &rng.vec(DIM), 71);
    let deleted: std::collections::HashSet<&String> = ids.iter().take(30).collect();
    assert!(
        all.iter().all(|id| !deleted.contains(id)),
        "deleted doc resurfaced from stale dump"
    );
    // 100 - 30 deleted + 1 winner = 71 live docs.
    assert_eq!(db.scan("docs").unwrap().len(), 71);
}

#[test]
fn corrupt_dump_falls_back_to_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(33);
    let q = rng.vec(DIM);
    let expected: Vec<String>;
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..200 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
        expected = search_ids(&db, &q, 10);
    }
    // Corrupt the dump body.
    let dump_path = vidx_file(&path);
    let mut bytes = std::fs::read(&dump_path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&dump_path, &bytes).unwrap();

    let db = Db::open(&path).unwrap();
    assert_eq!(
        search_ids(&db, &q, 10),
        expected,
        "rebuild produces correct results"
    );
    // And truncated garbage doesn't panic either.
    drop(db);
    std::fs::write(&dump_path, b"ESQLVIDXgarbage").unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(search_ids(&db, &q, 10), expected);
}

#[test]
fn compaction_refreshes_the_dump() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(44);
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        let mut ids = Vec::new();
        for i in 0..100 {
            ids.push(
                db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                    .unwrap(),
            );
        }
        for id in ids.iter().take(50) {
            db.delete("docs", id).unwrap();
        }
        let before = try_vidx_file(&path)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        db.compact().unwrap(); // dumps the compacted (tombstone-free) graph
        let after = std::fs::metadata(vidx_file(&path)).unwrap().len();
        assert!(after > 0);
        if before > 0 {
            assert!(
                after < before,
                "compacted dump should shrink: {before} -> {after}"
            );
        }
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 50);
    assert_eq!(search_ids(&db, &rng.vec(DIM), 50).len(), 50);
}

// --- Durable run sets ---------------------------------------------------------

fn vector_files(db_path: &Path, suffix: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(db_path.join("vectors"))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                .filter(|name| name.ends_with(suffix))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A tiny mutable-index pool makes the background publisher freeze and write
/// the HNSW overlay after a few hundred vectors, as a large ingest would.
fn small_delta_options() -> DbOptions {
    let mut options = DbOptions {
        auto_compaction: AutoCompactionOptions::disabled(),
        ..DbOptions::default()
    };
    options.memory.index_delta_pool_bytes = 192 * 1024;
    options
}

/// A pool that publishes two or three times over the test's inserts: enough
/// runs to prove durability, too few for the background merge policy (which
/// needs four comparable runs) to rewrite them while the test looks.
fn few_runs_options() -> DbOptions {
    let mut options = small_delta_options();
    options.memory.index_delta_pool_bytes = 768 * 1024;
    options
}

fn wait_for_publication(db: &Db, minimum: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while db.maintenance_stats().derived_publications < minimum {
        assert!(
            std::time::Instant::now() < deadline,
            "background vector publication did not happen"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn background_runs_survive_reopen_without_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(55);
    let q = rng.vec(DIM);
    let expected: Vec<String>;
    {
        let db = Db::create_with(&path, few_runs_options()).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..2_000 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
        wait_for_publication(&db, 1);
        // Keep writing after the publication so the close flushes a second
        // run on top of the background one.
        for i in 2_000..2_400 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
        expected = search_ids(&db, &q, 10);
    }
    let runs_before = vector_files(&path, ".vidx.run");
    assert!(
        runs_before.len() >= 2,
        "background and close flushes must both be durable runs: {runs_before:?}"
    );
    assert_eq!(
        vector_files(&path, ".vidx.runs").len(),
        1,
        "run manifest present"
    );
    assert!(
        vector_files(&path, ".vidx").is_empty(),
        "an index born from a background run has no legacy base"
    );

    let db = Db::open_with(&path, few_runs_options()).unwrap();
    assert_eq!(
        search_ids(&db, &q, 10),
        expected,
        "mapped run set answers identically"
    );
    assert_eq!(db.scan("docs").unwrap().len(), 2_400);
    drop(db);
    assert_eq!(
        vector_files(&path, ".vidx.run"),
        runs_before,
        "reopening maps the existing runs instead of rebuilding"
    );
    assert!(
        vector_files(&path, ".vidx").is_empty(),
        "a rebuild would have written a fresh base"
    );
}

#[test]
fn changes_between_durable_runs_are_replayed_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(66);
    let far = vec![7.0; DIM];
    let mut ids = Vec::new();

    // Session 1: sixty vectors, clean close -> base run.
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..60 {
            ids.push(
                db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                    .unwrap(),
            );
        }
    }
    assert_eq!(vector_files(&path, ".vidx").len(), 1);
    assert_eq!(vector_files(&path, ".vidx.runs").len(), 1);

    // Session 2: sixty more, ten deleted, ten lose their vector, ten move far
    // away. Clean close -> a second run; the older run still holds the stale
    // copies on disk.
    {
        let db = Db::open(&path).unwrap();
        for i in 60..120 {
            ids.push(
                db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                    .unwrap(),
            );
        }
        for id in &ids[0..10] {
            db.delete("docs", id).unwrap();
        }
        for id in &ids[10..20] {
            let mut patch = Record::new();
            patch.insert("embedding".into(), Value::Null);
            db.update("docs", id, patch).unwrap();
        }
        for id in &ids[20..30] {
            let mut patch = Record::new();
            patch.insert("embedding".into(), Value::Vector(far.clone()));
            db.update("docs", id, patch).unwrap();
        }
    }
    assert_eq!(
        vector_files(&path, ".vidx.run").len(),
        1,
        "close added one run"
    );

    // Session 3: the mapped set must not resurrect anything.
    let db = Db::open(&path).unwrap();
    let mut moved = search_ids(&db, &far, 10);
    moved.sort();
    let mut expected_moved: Vec<String> = ids[20..30].to_vec();
    expected_moved.sort();
    assert_eq!(
        moved, expected_moved,
        "updated vectors come from the newer run"
    );
    let all = search_ids(&db, &rng.vec(DIM), 200);
    assert_eq!(
        all.len(),
        100,
        "deleted and vector-less records are not indexed"
    );
    for id in &ids[0..20] {
        assert!(!all.contains(id), "{id} must not be searchable");
    }
    assert_eq!(db.scan("docs").unwrap().len(), 110);
}

#[test]
fn stale_or_broken_run_manifest_still_opens_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(77);
    let q = rng.vec(DIM);
    let manifest_path = |db_path: &Path| {
        db_path
            .join("vectors")
            .join(vector_files(db_path, ".vidx.runs").remove(0))
    };

    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..50 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
    }
    let stale_manifest = std::fs::read(manifest_path(&path)).unwrap();

    let expected: Vec<String>;
    {
        let db = Db::open(&path).unwrap();
        for i in 50..100 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
        expected = search_ids(&db, &q, 100);
    }
    assert_eq!(expected.len(), 100);

    // A manifest from before the last session (a crash before the clean
    // close) is a valid prefix: the newer vectors are replayed on open.
    std::fs::write(manifest_path(&path), &stale_manifest).unwrap();
    {
        let db = Db::open(&path).unwrap();
        assert_eq!(search_ids(&db, &q, 100), expected);
    }

    // A manifest that points at a missing run falls back and stays correct.
    let run = vector_files(&path, ".vidx.run").remove(0);
    std::fs::remove_file(path.join("vectors").join(run)).unwrap();
    {
        let db = Db::open(&path).unwrap();
        assert_eq!(search_ids(&db, &q, 100), expected);
    }

    // So does a corrupt manifest.
    std::fs::write(manifest_path(&path), b"ESQLDRN1garbage").unwrap();
    {
        let db = Db::open(&path).unwrap();
        assert_eq!(search_ids(&db, &q, 100), expected);
    }
    // The clean close after recovery leaves a consistent durable set again.
    let db = Db::open(&path).unwrap();
    assert_eq!(search_ids(&db, &q, 100), expected);
    drop(db);
    assert_eq!(vector_files(&path, ".vidx.runs").len(), 1);
}

#[test]
fn dropping_the_index_removes_its_runs_and_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(88);
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        for i in 0..40 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
    }
    {
        let db = Db::open(&path).unwrap();
        for i in 40..80 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM)))
                .unwrap();
        }
    }
    assert_eq!(vector_files(&path, ".vidx.run").len(), 1);
    let db = Db::open(&path).unwrap();
    db.drop_vector_index("docs", "embedding").unwrap();
    drop(db);
    assert!(vector_files(&path, ".vidx").is_empty());
    assert!(vector_files(&path, ".vidx.run").is_empty());
    assert!(vector_files(&path, ".vidx.runs").is_empty());
}

// --- Background run merges ------------------------------------------------------------

fn brute_force_top(vectors: &[(String, Vec<f32>)], q: &[f32], k: usize) -> Vec<String> {
    let mut scored: Vec<(f32, &str)> = vectors
        .iter()
        .map(|(id, v)| {
            let dot: f32 = q.iter().zip(v).map(|(a, b)| a * b).sum();
            let na: f32 = q.iter().map(|a| a * a).sum::<f32>().sqrt();
            let nb: f32 = v.iter().map(|b| b * b).sum::<f32>().sqrt();
            (1.0 - dot / (na * nb).max(f32::EPSILON), id.as_str())
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored
        .iter()
        .take(k)
        .map(|(_, id)| id.to_string())
        .collect()
}

#[test]
fn background_merges_bound_the_run_count_and_keep_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.esql");
    let mut rng = XorShift(99);
    let mut vectors: Vec<(String, Vec<f32>)> = Vec::new();
    let queries: Vec<Vec<f32>> = (0..20).map(|_| rng.vec(DIM)).collect();
    let far = vec![9.0; DIM];
    let before_close: Vec<Vec<String>>;
    let deleted: Vec<String>;
    {
        let db = Db::create_with(&path, small_delta_options()).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        // Enough vectors for several background publications; the tiny pool
        // makes every publication a run, so the merge policy has work.
        for i in 0..6_000 {
            let v = rng.vec(DIM);
            let id = db.insert("docs", doc(&format!("d{i}"), v.clone())).unwrap();
            vectors.push((id, v));
        }
        // Remove and reshape some vectors before the merge so the rebuilt run
        // must reflect them.
        deleted = vectors.drain(0..100).map(|(id, _)| id).collect();
        for id in &deleted {
            db.delete("docs", id).unwrap();
        }
        for (id, v) in vectors.iter_mut().take(50) {
            let mut patch = Record::new();
            patch.insert("embedding".into(), Value::Vector(far.clone()));
            db.update("docs", id, patch).unwrap();
            *v = far.clone();
        }
        wait_for_publication(&db, 1);
        db.wait_for_vector_run_merge().unwrap();
        let stats = db.maintenance_stats();
        assert!(
            stats.vector_run_merges >= 1,
            "at least one merge must have run: {stats:?}"
        );
        assert!(
            stats.vector_runs < stats.derived_publications as usize + 1,
            "merging must leave fewer runs than publications: {stats:?}"
        );

        let mut hits = 0usize;
        for q in &queries {
            let found = search_ids(&db, q, 10);
            let truth = brute_force_top(&vectors, q, 10);
            hits += found.iter().filter(|id| truth.contains(id)).count();
        }
        assert!(hits >= 180, "recall@10 after merging is {hits}/200");
        let mut moved = search_ids(&db, &far, 50);
        moved.sort();
        let mut expected_moved: Vec<String> =
            vectors[0..50].iter().map(|(id, _)| id.clone()).collect();
        expected_moved.sort();
        assert_eq!(moved, expected_moved, "reshaped vectors survive the merge");
        let all = search_ids(&db, &rng.vec(DIM), 6_000);
        assert!(
            all.len() >= 5_850,
            "live vectors remain searchable: {}",
            all.len()
        );
        for id in &deleted {
            assert!(!all.contains(id), "{id} was deleted before the merge");
        }
        before_close = queries.iter().map(|q| search_ids(&db, q, 10)).collect();
    }
    // The close flushed the remaining overlay as one more durable run.
    let runs_after_merge = vector_files(&path, ".vidx.run");
    // A reopen maps the durable set and may keep merging it; whatever it
    // does, the results stay correct and disk agrees with the mapped runs.
    let db = Db::open_with(&path, small_delta_options()).unwrap();
    db.wait_for_vector_run_merge().unwrap();
    let mut hits = 0usize;
    for (q, before) in queries.iter().zip(&before_close) {
        let found = search_ids(&db, q, 10);
        let truth = brute_force_top(&vectors, q, 10);
        hits += found.iter().filter(|id| truth.contains(id)).count();
        assert!(!found.is_empty() && before.len() == found.len());
    }
    assert!(hits >= 180, "recall@10 after reopening is {hits}/200");
    let mut moved = search_ids(&db, &far, 50);
    moved.sort();
    let mut expected_moved: Vec<String> = vectors[0..50].iter().map(|(id, _)| id.clone()).collect();
    expected_moved.sort();
    assert_eq!(moved, expected_moved);
    let all = search_ids(&db, &rng.vec(DIM), 6_000);
    for id in &deleted {
        assert!(!all.contains(id), "{id} stays deleted after a reopen");
    }
    let runs_now = db.maintenance_stats().vector_runs;
    assert!(runs_now <= runs_after_merge.len());
    drop(db);
    assert_eq!(
        vector_files(&path, ".vidx.run").len(),
        runs_now,
        "the files on disk are exactly the mapped runs"
    );
}
