//! A commit must not pay for everything written since the last publication.
//!
//! It used to. Once the shared delta pool was half full, every commit walked
//! the derived overlays end to end to decide whether a background publication
//! was due, and that walk grows with the overlays themselves. On the shop
//! simulation the write operations doubled in latency between checkpoints and
//! snapped back after each one: `session_check` went from 26 to 90 µs and
//! back, while the read-only operations stayed flat.
//!
//! Timing that is too noisy to assert on, so this counts the walks instead.
use elitesql_core::{Db, DbOptions, Durability, MemoryOptions, Value};

#[test]
fn a_commit_does_not_measure_the_derived_overlays() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("flat.esql"),
        DbOptions {
            durability: Durability::Balanced,
            memory: MemoryOptions {
                // Small enough that the half-full threshold, past which the
                // walk used to happen on every commit, is crossed early.
                index_delta_pool_bytes: 4 * 1024 * 1024,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query(
        "CREATE TABLE events (token text NOT NULL, owner int64 NOT NULL, \
         topic text NOT NULL, note text NOT NULL)",
    )
    .unwrap();
    db.query("CREATE UNIQUE INDEX ON events (token)").unwrap();
    db.query("CREATE INDEX ON events (owner)").unwrap();
    db.query("CREATE INDEX ON events (topic)").unwrap();

    const COMMITS: i64 = 20_000;
    for n in 0..COMMITS {
        db.query_params(
            "INSERT INTO events (token, owner, topic, note) VALUES (?, ?, ?, ?)",
            &[
                Value::Text(format!("tok-{n:09}")),
                Value::Int64(n % 500),
                Value::Text(format!("topic-{}", n % 40)),
                Value::Text(format!("note {n}")),
            ],
        )
        .unwrap();
    }
    assert_eq!(db.scan("events").unwrap().len(), COMMITS as usize);

    // The scheduling decision samples: at most one walk every 256 commits,
    // plus the ones the memory-pressure path and an actual publication make,
    // which are bounded by how often those happen. Before the fix this was
    // one walk per commit.
    let walks = db.maintenance_stats().derived_size_walks;
    assert!(
        walks * 8 < COMMITS as u64,
        "the commit path measured the derived overlays {walks} times in {COMMITS} commits"
    );
}
