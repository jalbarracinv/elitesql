//! SQL three-valued logic.
//!
//! A comparison against NULL is UNKNOWN, not false, and a row survives a WHERE
//! clause only when the predicate is TRUE. The cases here are exactly the ones
//! where UNKNOWN and false disagree — `NOT`, `OR`, and a NULL inside an `IN`
//! list — and every expectation matches standard SQL and MySQL.

use elitesql_core::{Db, QueryOutput, Value};
use tempfile::TempDir;

fn seeded() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("nulls.esql")).unwrap();
    db.query("CREATE TABLE t (name text, n int64)").unwrap();
    db.query("INSERT INTO t (name, n) VALUES ('a', 1), ('b', 2), (NULL, NULL)")
        .unwrap();
    (dir, db)
}

/// The values of column `n` a query returns, NULL rendered as -1 so the
/// expectations below read as plain lists.
fn ns(db: &Db, sql: &str) -> Vec<i64> {
    let QueryOutput::Rows { rows, .. } = db.query(sql).unwrap() else {
        panic!("expected rows from {sql}");
    };
    let mut out: Vec<i64> = rows
        .into_iter()
        .map(|row| match row.into_iter().next() {
            Some(Value::Int64(n)) => n,
            Some(Value::Null) => -1,
            other => panic!("expected int64 or null, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

#[test]
fn a_comparison_against_null_never_keeps_the_row() {
    let (_d, db) = seeded();
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n = 1"), [1]);
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n <> 1"), [2]);
    // `= NULL` is UNKNOWN for every row, including the NULL one.
    assert!(ns(&db, "SELECT n FROM t WHERE n = NULL").is_empty());
    assert!(ns(&db, "SELECT n FROM t WHERE n <> NULL").is_empty());
}

/// The case that separates three-valued logic from two-valued: NOT UNKNOWN is
/// UNKNOWN, so negating a comparison must not resurrect the NULL row.
#[test]
fn not_of_an_unknown_comparison_stays_unknown() {
    let (_d, db) = seeded();
    assert_eq!(ns(&db, "SELECT n FROM t WHERE NOT n = 1"), [2]);
    assert_eq!(ns(&db, "SELECT n FROM t WHERE NOT name = 'a'"), [2]);
    assert_eq!(
        ns(&db, "SELECT n FROM t WHERE NOT (n = 1 OR n = 2)"),
        Vec::<i64>::new()
    );
    // Double negation is still not a way back in.
    assert_eq!(ns(&db, "SELECT n FROM t WHERE NOT NOT n = 1"), [1]);
}

#[test]
fn is_null_always_has_a_definite_answer() {
    let (_d, db) = seeded();
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n IS NULL"), [-1]);
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n IS NOT NULL"), [1, 2]);
    assert_eq!(ns(&db, "SELECT n FROM t WHERE NOT n IS NULL"), [1, 2]);
    // IS NULL is how a NULL row is deliberately included again.
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n = 1 OR n IS NULL"), [-1, 1]);
}

#[test]
fn and_and_or_follow_the_three_valued_truth_tables() {
    let (_d, db) = seeded();
    // FALSE AND UNKNOWN is FALSE, so a false conjunct settles the row.
    assert_eq!(
        ns(&db, "SELECT n FROM t WHERE n = 99 AND name = 'a'"),
        Vec::<i64>::new()
    );
    // TRUE OR UNKNOWN is TRUE.
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n IS NULL OR n = 1"), [-1, 1]);
    // UNKNOWN AND TRUE is UNKNOWN: the NULL row is still dropped.
    assert_eq!(
        ns(&db, "SELECT n FROM t WHERE n = 1 AND name IS NOT NULL"),
        [1]
    );
}

/// A NULL inside the list means "no match" cannot be trusted: the NULL might
/// have been the match, so the answer is UNKNOWN rather than false.
#[test]
fn a_null_in_an_in_list_makes_a_miss_unknown() {
    let (_d, db) = seeded();
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n IN (1, 2)"), [1, 2]);
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n NOT IN (1, 5)"), [2]);

    // A hit is still TRUE even with a NULL in the list.
    assert_eq!(ns(&db, "SELECT n FROM t WHERE n IN (1, NULL)"), [1]);
    // A miss alongside a NULL is UNKNOWN, so neither IN nor NOT IN keeps it.
    assert_eq!(
        ns(&db, "SELECT n FROM t WHERE n IN (99, NULL)"),
        Vec::<i64>::new()
    );
    assert_eq!(
        ns(&db, "SELECT n FROM t WHERE n NOT IN (99, NULL)"),
        Vec::<i64>::new()
    );
    assert_eq!(
        ns(&db, "SELECT n FROM t WHERE n NOT IN (1, NULL)"),
        Vec::<i64>::new()
    );
}

#[test]
fn having_drops_unknown_groups_too() {
    let (_d, db) = seeded();
    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT name, count(*) FROM t GROUP BY name HAVING max(n) > 1")
        .unwrap()
    else {
        panic!("expected rows");
    };
    // The NULL-name group aggregates max(n) = NULL, so the comparison is
    // UNKNOWN and the group does not pass HAVING.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Text("b".into()));
}

#[test]
fn the_same_rules_hold_through_an_index_and_a_join() {
    let (_d, db) = seeded();
    db.query("CREATE INDEX ON t (n)").unwrap();
    // An indexed column must not take a different path through the logic.
    assert_eq!(ns(&db, "SELECT n FROM t WHERE NOT n = 1"), [2]);
    assert!(ns(&db, "SELECT n FROM t WHERE n = NULL").is_empty());

    db.query("CREATE TABLE u (ref int64, tag text)").unwrap();
    db.query("INSERT INTO u (ref, tag) VALUES (1, 'x'), (2, NULL)")
        .unwrap();
    let QueryOutput::Rows { rows, .. } = db
        .query("SELECT t.n FROM t JOIN u ON u.ref = t.n WHERE NOT u.tag = 'x'")
        .unwrap()
    else {
        panic!("expected rows");
    };
    // u.tag is NULL for ref = 2, so the negated comparison is UNKNOWN and the
    // joined row is dropped rather than kept.
    assert!(rows.is_empty(), "got {rows:?}");
}
