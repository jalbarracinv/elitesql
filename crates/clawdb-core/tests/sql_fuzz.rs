//! Parser/executor fuzzing: random garbage and mutated valid queries must
//! never panic — every input returns Ok or a clean Err. Includes a deep
//! nesting case that must hit the depth guard, not the stack limit.

use clawdb_core::{Db, Value};

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

const CORPUS: &[&str] = &[
    "SELECT * FROM docs WHERE a = 1 AND b < 2.5 OR NOT c IS NULL ORDER BY a DESC LIMIT 10 OFFSET 2",
    "SELECT d.a, e.b AS x FROM docs d LEFT JOIN extra e ON e.doc_id = d.id WHERE d.a IN (1, 2, 3)",
    "INSERT INTO docs (a, b, c) VALUES (1, 2.5, 'text'), (-7, 0.0, NULL)",
    "UPDATE docs SET a = 5, c = 'y' WHERE b >= 1.5 AND c <> 'z'",
    "DELETE FROM docs WHERE a NOT IN (1, 2) OR c IS NOT NULL",
    "CREATE TABLE t2 (a int64 NOT NULL, b float64, c text, d blob, e timestamp, f json)",
    "CREATE UNIQUE INDEX ON docs (c)",
    "SELECT * FROM docs RIGHT JOIN extra ON extra.doc_id = docs.id",
    "INSERT INTO docs (d) VALUES (X'00FF')",
    "SELECT c, count(*) AS n, sum(a) FROM docs GROUP BY c HAVING count(*) > 1 ORDER BY n DESC",
    "SELECT count(*), min(b), max(c), avg(a) FROM docs WHERE a IN (1, 2, 3)",
    "CREATE TABLE dt (d date, t time)",
    "INSERT INTO dt (d, t) VALUES ('2026-08-07', '09:30:00.5'), (NULL, NULL)",
    "SELECT d FROM dt WHERE d >= '2026-01-01' AND t < '18:00:00' ORDER BY d",
];

fn setup() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("fuzz.clawdb")).unwrap();
    db.query("CREATE TABLE docs (a int64, b float64, c text, d blob)").unwrap();
    db.query("CREATE TABLE extra (doc_id text, b int64)").unwrap();
    db.query("INSERT INTO docs (a, b, c) VALUES (1, 1.5, 'x'), (2, 2.5, 'y')").unwrap();
    (dir, db)
}

#[test]
fn random_garbage_never_panics() {
    let (_d, db) = setup();
    let mut rng = XorShift(0xC0FFEE);
    let charset: &[u8] = b"SELECTFROMWHEREINSERTUPDATEDELETEJOINONANDORNOT()*,.;'=<>!0123456789abc xyz_\"%+-";
    for _ in 0..3000 {
        let len = rng.below(120) as usize;
        let s: String = (0..len)
            .map(|_| charset[rng.below(charset.len() as u64) as usize] as char)
            .collect();
        let _ = db.query(&s); // Ok or Err, never panic
    }
}

#[test]
fn mutated_corpus_never_panics() {
    let (_d, db) = setup();
    let mut rng = XorShift(0xBEEF);
    for round in 0..3000 {
        let base = CORPUS[round % CORPUS.len()];
        let mut bytes = base.as_bytes().to_vec();
        match rng.below(3) {
            0 => {
                // flip random bytes
                for _ in 0..=rng.below(4) {
                    let i = rng.below(bytes.len() as u64) as usize;
                    bytes[i] = (rng.below(94) + 32) as u8;
                }
            }
            1 => {
                // truncate
                let cut = rng.below(bytes.len() as u64) as usize;
                bytes.truncate(cut);
            }
            _ => {
                // duplicate a slice into the middle
                let a = rng.below(bytes.len() as u64) as usize;
                let b = (a + rng.below(20) as usize).min(bytes.len());
                let slice: Vec<u8> = bytes[a..b].to_vec();
                bytes.extend_from_slice(&slice);
            }
        }
        if let Ok(s) = String::from_utf8(bytes) {
            let _ = db.query(&s);
        }
    }
}

#[test]
fn deep_nesting_hits_guard_not_stack() {
    let (_d, db) = setup();
    let deep = format!(
        "SELECT * FROM docs WHERE {}a = 1{}",
        "(".repeat(500),
        ")".repeat(500)
    );
    let err = db.query(&deep).unwrap_err();
    assert!(err.to_string().contains("deeply nested"), "{err}");

    let many_nots = format!("SELECT * FROM docs WHERE {} a = 1", "NOT ".repeat(500));
    let err = db.query(&many_nots).unwrap_err();
    assert!(err.to_string().contains("deeply nested"), "{err}");
}

#[test]
fn valid_corpus_still_valid_after_setup() {
    let (_d, db) = setup();
    // Sanity: the corpus statements themselves execute or fail cleanly.
    // Some of them mutate or delete data — that's fine; what matters is
    // that the database stays consistent and usable afterwards.
    for sql in CORPUS {
        let _ = db.query(sql);
    }
    db.query("INSERT INTO docs (a, c) VALUES (777, 'alive')").unwrap();
    let out = db.query("SELECT c FROM docs WHERE a = 777").unwrap();
    match out {
        clawdb_core::QueryOutput::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Text("alive".into()));
        }
        other => panic!("unexpected {other:?}"),
    }
}
