//! Where the time of one small statement goes.
//!
//! The scale benchmark measures bulk transactional ingest; this one measures
//! the per-statement path an operational workload lives on, layer by layer:
//! the storage primitive, the SQL executor above it, and the JSON boundary the
//! C ABI (and therefore every binding) crosses.
//!
//! `cargo run --release -p elitesql-core --example statement_cost`
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use elitesql_core::{jsonio, Db, DbOptions, Durability, QueryOutput, Value};

/// Counts every allocation the process makes, so a statement's cost can be
/// expressed in allocations rather than in profiler samples. Sampling says
/// *that* a path allocates; this says how many times and lets a change be
/// checked against a count instead of against a noisy microsecond.
struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Allocations and bytes one call makes, averaged over `iterations`.
fn allocations(label: &str, iterations: u32, mut body: impl FnMut()) -> (f64, f64) {
    body();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    for _ in 0..iterations {
        body();
    }
    let count = (ALLOCATIONS.load(Ordering::Relaxed) - before) as f64 / f64::from(iterations);
    let bytes =
        (ALLOCATED_BYTES.load(Ordering::Relaxed) - before_bytes) as f64 / f64::from(iterations);
    println!("{label:<52} {count:8.1} allocs {bytes:9.0} B");
    (count, bytes)
}

fn bench(label: &str, iterations: u32, mut body: impl FnMut()) -> f64 {
    body();
    let started = Instant::now();
    for _ in 0..iterations {
        body();
    }
    let per_call = started.elapsed().as_secs_f64() * 1e6 / f64::from(iterations);
    println!("{label:<52} {per_call:8.2} us");
    per_call
}

fn main() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Db::create_with(
        dir.path().join("cost.esql"),
        DbOptions {
            durability: Durability::Balanced,
            ..DbOptions::default()
        },
    )
    .expect("create");
    db.query(
        "CREATE TABLE p (id int AUTO_INCREMENT PRIMARY KEY, name text NOT NULL, \
         price int NOT NULL, bucket int NOT NULL, rare int NOT NULL)",
    )
    .expect("ddl");
    db.query("CREATE INDEX ON p (name)").expect("index");
    // An integer index beside the text one, so an ablation can drive a
    // statement without also decoding a string for every row it matches.
    db.query("CREATE INDEX ON p (bucket)")
        .expect("bucket index");
    let mut txn = db.begin();
    for i in 0..20_000 {
        txn.query_params(
            "INSERT INTO p (name, price, bucket, rare) VALUES (?, ?, ?, ?)",
            &[
                Value::Text(format!("n{}", i % 500)),
                Value::Int64(i),
                Value::Int64(i % 500),
                Value::Int64(i),
            ],
        )
        .expect("seed");
    }
    txn.commit().expect("seed commit");
    db.checkpoint().expect("checkpoint");
    {
        // Every keyed lookup pays the page search once per published run, so
        // the decomposition below only means anything with the run count
        // beside it.
        let stats = db.maintenance_stats();
        println!(
            "published runs: {} primary, {} secondary",
            stats.primary_runs, stats.secondary_runs
        );
    }

    // The physical key of the row whose declared id is 7, for the raw
    // storage primitive (which addresses rows by their physical id).
    let QueryOutput::Rows { rows, .. } = db.query("SELECT id FROM p WHERE id = 7").expect("probe")
    else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    let physical = db
        .scan_batch("p", None, 8)
        .expect("scan")
        .into_iter()
        .find(|(_, record)| record.get("price") == Some(&Value::Int64(6)))
        .map(|(id, _)| id)
        .expect("row");

    let select = "SELECT id, name, price FROM p WHERE id = ?";
    let params = [Value::Int64(7)];
    println!("\n--- point read, one row of three columns ---");
    let raw = bench("Db::get (storage primitive, no SQL)", 20_000, || {
        db.get("p", &physical).expect("get");
    });
    let sql = bench(
        "Db::query_params -> QueryOutput (SQL executor)",
        20_000,
        || {
            db.query_params(select, &params).expect("query");
        },
    );
    let json = bench("jsonio JSON path (what the C ABI returns)", 20_000, || {
        let out = jsonio::query_with_params_json_bounded(
            &db,
            select,
            Some(&serde_json::json!([7])),
            10_000,
        )
        .expect("json query");
        let _ = jsonio::output_to_json(&out).to_string();
    });
    println!(
        "{:<52} {:8.2} us",
        "  SQL layer above the storage primitive",
        sql - raw
    );
    println!(
        "{:<52} {:8.2} us",
        "  JSON boundary above the SQL layer",
        json - sql
    );

    let wide = row_and_column_cost(dir.path());

    // `--profile <seconds>` loops the point read so an external sampler can
    // attribute the SQL layer's cost to functions.
    if let Some(seconds) = std::env::args()
        .skip_while(|argument| argument != "--profile")
        .nth(1)
        .and_then(|value| value.parse::<f64>().ok())
    {
        let which = std::env::args().nth(3).unwrap_or_else(|| "point".into());
        println!(
            "\nlooping '{which}' for {seconds} s (pid {})",
            std::process::id()
        );
        let until = Instant::now() + std::time::Duration::from_secs_f64(seconds);
        while Instant::now() < until {
            for _ in 0..200 {
                match which.as_str() {
                    "scan" => {
                        wide.query("SELECT c0, c1, c2 FROM w").expect("scan");
                    }
                    "indexed" => {
                        db.query_params(
                            "SELECT id, price FROM p WHERE name = ? ORDER BY price LIMIT 20",
                            &[Value::Text("n42".into())],
                        )
                        .expect("indexed");
                    }
                    "update" => {
                        db.query_params("UPDATE p SET price = price + 1 WHERE id = ?", &params)
                            .expect("update");
                    }
                    _ => {
                        db.query_params(select, &params).expect("query");
                    }
                }
            }
        }
        return;
    }

    // Where an indexed read's time goes, by ablation rather than by profile:
    // each line adds one stage to the one above it, over the same 40 rows.
    // `COUNT(*)` streams its rows rather than collecting them, but the batch
    // it streams from still owns an id and a row pair for each, so the first
    // line is what reaching a row and handing it to the executor costs, not
    // the directory lookup alone.
    println!("\n--- indexed read of 40 rows, stage by stage ---");
    let matched = 40.0;
    let counted = bench(
        "COUNT(*): reached and handed over, not decoded",
        5_000,
        || {
            db.query_params(
                "SELECT count(*) FROM p WHERE name = ?",
                &[Value::Text("n42".into())],
            )
            .expect("count");
        },
    );
    let single = bench("COUNT(*) of one row, by identity", 5_000, || {
        db.query_params("SELECT count(*) FROM p WHERE id = ?", &[Value::Int64(7)])
            .expect("count one");
    });
    let one = bench("+ one column decoded and returned", 5_000, || {
        db.query_params(
            "SELECT price FROM p WHERE name = ?",
            &[Value::Text("n42".into())],
        )
        .expect("one column");
    });
    let two = bench("+ a second column", 5_000, || {
        db.query_params(
            "SELECT id, price FROM p WHERE bucket = ?",
            &[Value::Int64(42)],
        )
        .expect("two columns");
    });
    let sorted = bench("+ ORDER BY price, LIMIT 20", 5_000, || {
        db.query_params(
            "SELECT id, price FROM p WHERE bucket = ? ORDER BY price LIMIT 20",
            &[Value::Int64(42)],
        )
        .expect("sorted");
    });
    for (label, us) in [
        ("  fixed cost of the statement", single),
        (
            "  reaching a row and handing it over, per row",
            (counted - single) / (matched - 1.0),
        ),
        ("  first column, per row", (one - counted) / matched),
        ("  second column, per row", (two - one) / matched),
        ("  sort and limit, per row", (sorted - two) / matched),
    ] {
        println!("{label:<52} {:8.0} ns", us * 1000.0);
    }

    // The same rows, never checkpointed, so every lookup answers from the
    // resident delta instead of a published run.
    {
        let resident = Db::create_with(
            dir.path().join("resident.esql"),
            DbOptions {
                durability: Durability::Balanced,
                ..DbOptions::default()
            },
        )
        .expect("create resident");
        resident
            .query("CREATE TABLE p (id int AUTO_INCREMENT PRIMARY KEY, name text NOT NULL, price int NOT NULL)")
            .expect("ddl");
        resident.query("CREATE INDEX ON p (name)").expect("index");
        let mut txn = resident.begin();
        for i in 0..20_000 {
            txn.query_params(
                "INSERT INTO p (name, price) VALUES (?, ?)",
                &[Value::Text(format!("n{}", i % 500)), Value::Int64(i)],
            )
            .expect("seed");
        }
        txn.commit().expect("seed commit");
        println!("\n--- the same 40 rows from the resident delta ---");
        let single = bench("COUNT(*) of one row, by identity", 5_000, || {
            resident
                .query_params("SELECT count(*) FROM p WHERE id = ?", &[Value::Int64(7)])
                .expect("count one");
        });
        let counted = bench(
            "COUNT(*): reached and handed over, not decoded",
            5_000,
            || {
                resident
                    .query_params(
                        "SELECT count(*) FROM p WHERE name = ?",
                        &[Value::Text("n42".into())],
                    )
                    .expect("count");
            },
        );
        println!(
            "{:<52} {:8.0} ns",
            "  reaching a row and handing it over, per row",
            (counted - single) / (matched - 1.0) * 1000.0
        );

        // `--profile <seconds> resident` loops that statement so a sampler can
        // attribute the part of a keyed read that memory alone still costs.
        if let Some(seconds) = std::env::args()
            .skip_while(|argument| argument != "--profile")
            .nth(1)
            .and_then(|value| value.parse::<f64>().ok())
        {
            if std::env::args().nth(3).as_deref() == Some("resident") {
                println!(
                    "\nlooping the resident keyed read for {seconds} s (pid {})",
                    std::process::id()
                );
                let until = Instant::now() + std::time::Duration::from_secs_f64(seconds);
                while Instant::now() < until {
                    for _ in 0..200 {
                        resident
                            .query_params(
                                "SELECT count(*) FROM p WHERE name = ?",
                                &[Value::Text("n42".into())],
                            )
                            .expect("count");
                    }
                }
                return;
            }
        }

        // The same forty matched rows, in a table small enough that every
        // search is trivial, and published like the big one. If the cost per
        // row does not move, the search is not what a matched row costs.
        //
        // This table used to be left unpublished, which made it a second
        // measurement of the resident overlay rather than of a small run, and
        // its 133 ns was read as evidence that the page directory was the
        // cost. It was not.
        let small = Db::create_with(
            dir.path().join("small.esql"),
            DbOptions {
                durability: Durability::Balanced,
                ..DbOptions::default()
            },
        )
        .expect("create small");
        small
            .query("CREATE TABLE p (id int AUTO_INCREMENT PRIMARY KEY, name text NOT NULL, price int NOT NULL)")
            .expect("ddl small");
        small.query("CREATE INDEX ON p (name)").expect("index");
        let mut txn = small.begin();
        for i in 0..200 {
            txn.query_params(
                "INSERT INTO p (name, price) VALUES (?, ?)",
                &[Value::Text(format!("n{}", i % 5)), Value::Int64(i)],
            )
            .expect("seed small");
        }
        txn.commit().expect("commit small");
        small.checkpoint().expect("checkpoint small");
        let single = bench("COUNT(*) of one row, 200-row table", 5_000, || {
            small
                .query_params("SELECT count(*) FROM p WHERE id = ?", &[Value::Int64(7)])
                .expect("count one");
        });
        let counted = bench("COUNT(*) of 40 rows, 200-row table", 5_000, || {
            small
                .query_params(
                    "SELECT count(*) FROM p WHERE name = ?",
                    &[Value::Text("n2".into())],
                )
                .expect("count");
        });
        println!(
            "{:<52} {:8.0} ns",
            "  reaching a row and handing it over, per row",
            (counted - single) / (matched - 1.0) * 1000.0
        );
        println!(
            "  in {} published run(s)",
            small.maintenance_stats().primary_runs
        );
    }

    // Every statement clones the whole table schema out of the catalog before
    // it can resolve a column. This is what that costs, against the statement
    // it is part of.
    // Allocations, counted rather than sampled. Every shape is measured twice
    // with the same statement text and driver, once matching one row through
    // the unique `rare` index and once matching forty through `bucket`, so
    // the subtraction removes that shape's own fixed cost and nothing else.
    println!("\n--- allocations a matched row costs, counted ---");
    let per_row = |label: &str, select: &str| {
        let one = {
            let sql = format!("{select} FROM p WHERE rare = ?");
            let mut count = 0.0;
            count += allocations(&format!("{label}, one row"), 2_000, || {
                db.query_params(&sql, &[Value::Int64(7)]).expect("one");
            })
            .0;
            count
        };
        let forty = {
            let sql = format!("{select} FROM p WHERE bucket = ?");
            allocations(&format!("{label}, forty rows"), 2_000, || {
                db.query_params(&sql, &[Value::Int64(42)]).expect("forty");
            })
            .0
        };
        println!(
            "{:<52} {:8.2} allocs",
            "  per matched row",
            (forty - one) / 39.0
        );
    };
    per_row("COUNT(*)", "SELECT count(*)");
    per_row("one int column", "SELECT price");
    per_row("two int columns", "SELECT id, price");
    per_row("every column, including the text one", "SELECT *");
    per_row("SUM of an int column", "SELECT sum(price)");
    per_row("the identity column alone", "SELECT id");
    per_row("the driver column alone", "SELECT bucket");
    per_row("two ints, neither the identity", "SELECT price, bucket");
    per_row("the text column alone", "SELECT name");

    // A statement's fixed cost, in allocations: the storage primitive under it,
    // the same row through SQL, and the same SQL run twice to show that
    // nothing is cached between executions.
    println!("\n--- allocations a statement pays whatever it reads ---");
    allocations("Db::get: the storage primitive", 5_000, || {
        db.get("p", &physical).expect("get");
    });
    allocations("SELECT one row by identity", 5_000, || {
        db.query_params("SELECT price FROM p WHERE id = ?", &[Value::Int64(7)])
            .expect("point");
    });
    allocations("the same SELECT with no parameters", 5_000, || {
        db.query("SELECT price FROM p WHERE id = 7").expect("point");
    });
    for (label, sql) in [
        ("no WHERE, LIMIT 1", "SELECT price FROM p LIMIT 1"),
        ("WHERE on the identity", "SELECT price FROM p WHERE id = 7"),
        ("two columns", "SELECT id, price FROM p WHERE id = 7"),
        (
            "four columns",
            "SELECT id, name, price, bucket FROM p WHERE id = 7",
        ),
        (
            "WHERE plus ORDER BY",
            "SELECT price FROM p WHERE id = 7 ORDER BY price",
        ),
        (
            "two conjuncts",
            "SELECT price FROM p WHERE id = 7 AND price >= 0",
        ),
    ] {
        allocations(&format!("  {label}"), 5_000, || {
            db.query(sql).expect("shape");
        });
    }
    allocations("INSERT one row", 2_000, || {
        db.query_params(
            "INSERT INTO p (name, price, bucket, rare) VALUES (?, ?, ?, ?)",
            &[
                Value::Text("x".into()),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(-1),
            ],
        )
        .expect("insert");
    });

    // An autocommit INSERT is a statement plus a whole commit. Batching a
    // hundred of them into one transaction divides the commit across them, so
    // the difference is what committing costs and what the statement costs.
    let batched = {
        let started = ALLOCATIONS.load(Ordering::Relaxed);
        for _ in 0..20 {
            let mut txn = db.begin();
            for i in 0..100i64 {
                txn.query_params(
                    "INSERT INTO p (name, price, bucket, rare) VALUES (?, ?, ?, ?)",
                    &[
                        Value::Text("x".into()),
                        Value::Int64(i),
                        Value::Int64(1),
                        Value::Int64(-2 - i),
                    ],
                )
                .expect("staged insert");
            }
            txn.commit().expect("commit");
        }
        (ALLOCATIONS.load(Ordering::Relaxed) - started) as f64 / 2_000.0
    };
    println!(
        "{:<52} {batched:8.1} allocs",
        "INSERT staged in a transaction of 100"
    );

    println!("\n--- what a statement pays before it reads anything ---");
    bench(
        "Db::table_schema: one clone of the catalog entry",
        20_000,
        || {
            let _ = db.table_schema("p").expect("schema");
        },
    );

    // The shop simulation's `browse` page, shaped exactly as the workload
    // issues it: an index equality that matches a few hundred rows, sorted,
    // twenty returned. Measured in the engine alone and across the JSON the C
    // ABI returns, so the harness's Python share can be read off against it.
    {
        let shop = Db::create_with(
            dir.path().join("shop.esql"),
            DbOptions {
                durability: Durability::Balanced,
                ..DbOptions::default()
            },
        )
        .expect("create shop");
        shop.query(
            "CREATE TABLE products (id int AUTO_INCREMENT PRIMARY KEY, sku text NOT NULL, \
             name text NOT NULL, description text NOT NULL, category text NOT NULL, \
             price_cents int NOT NULL, stock int NOT NULL, sold int NOT NULL DEFAULT 0)",
        )
        .expect("ddl shop");
        shop.query("CREATE INDEX ON products (category)")
            .expect("index");
        let mut txn = shop.begin();
        for i in 0..5_000i64 {
            txn.query_params(
                "INSERT INTO products (sku, name, description, category, price_cents, stock) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    Value::Text(format!("SKU-{i:06}")),
                    Value::Text(format!("Product {i}")),
                    Value::Text(format!(
                        "A description of product {i}, long enough to matter"
                    )),
                    Value::Text(format!("cat-{}", i % 14)),
                    Value::Int64(100 + i % 9_900),
                    Value::Int64(25),
                ],
            )
            .expect("seed shop");
        }
        txn.commit().expect("commit shop");

        println!("\n--- the shop's browse page, 339 rows read, 20 returned ---");
        let resident = bench("before a checkpoint, rows in the overlay", 2_000, || {
            shop.query_params(
                "SELECT id, name, price_cents, stock FROM products WHERE category = ? \
                 ORDER BY price_cents ASC LIMIT ? OFFSET ?",
                &[
                    Value::Text("cat-3".into()),
                    Value::Int64(20),
                    Value::Int64(40),
                ],
            )
            .expect("browse");
        });
        let _ = resident;
        shop.checkpoint().expect("checkpoint shop");
        let browse = "SELECT id, name, price_cents, stock FROM products WHERE category = ? \
                      ORDER BY price_cents ASC LIMIT ? OFFSET ?";
        let params = [
            Value::Text("cat-3".into()),
            Value::Int64(20),
            Value::Int64(40),
        ];
        let engine = bench("the engine alone", 2_000, || {
            shop.query_params(browse, &params).expect("browse");
        });
        let json = bench("across the JSON the C ABI returns", 2_000, || {
            let out = jsonio::query_with_params_json_bounded(
                &shop,
                browse,
                Some(&serde_json::json!(["cat-3", 20, 40])),
                10_000,
            )
            .expect("browse json");
            let _ = jsonio::output_to_json(&out).to_string();
        });
        println!(
            "{:<52} {:8.1} us",
            "  the JSON boundary adds",
            json - engine
        );
    }

    println!("\n--- other statements (SQL executor, no JSON) ---");
    bench(
        "SELECT by indexed column, ORDER BY, LIMIT 20",
        5_000,
        || {
            db.query_params(
                "SELECT id, price FROM p WHERE name = ? ORDER BY price LIMIT 20",
                &[Value::Text("n42".into())],
            )
            .expect("indexed");
        },
    );
    bench("UPDATE by id", 5_000, || {
        db.query_params("UPDATE p SET price = price + 1 WHERE id = ?", &params)
            .expect("update");
    });
    {
        // Where an autocommit write's time goes. A point UPDATE measured
        // 16.7 us against SQLite's 1.9 through the same Python driver, the
        // widest ratio left in the shop mix, so the commit's own phases are
        // worth printing beside it.
        let before = db.maintenance_stats();
        let rounds = 5_000u32;
        for _ in 0..rounds {
            db.query_params("UPDATE p SET price = price + 1 WHERE id = ?", &params)
                .expect("update");
        }
        let after = db.maintenance_stats();
        let commits = after.commits.saturating_sub(before.commits).max(1);
        let per = |now: std::time::Duration, was: std::time::Duration| {
            now.saturating_sub(was).as_secs_f64() * 1e6 / commits as f64
        };
        println!("\n--- where one autocommit commit's time goes ---");
        for (label, now, was) in [
            ("whole commit", after.commit_time, before.commit_time),
            (
                "  waiting for the serialization lock",
                after.commit_lock_wait_time,
                before.commit_lock_wait_time,
            ),
            (
                "  holding it",
                after.commit_lock_hold_time,
                before.commit_lock_hold_time,
            ),
            (
                "  preparing",
                after.commit_phase_prepare_time,
                before.commit_phase_prepare_time,
            ),
            (
                "  encoding the record",
                after.commit_phase_record_encode_time,
                before.commit_phase_record_encode_time,
            ),
            (
                "  encoding the WAL frame",
                after.commit_phase_wal_encode_time,
                before.commit_phase_wal_encode_time,
            ),
            (
                "  validating",
                after.commit_phase_validation_time,
                before.commit_phase_validation_time,
            ),
            (
                "  WAL append and sync",
                after.commit_wal_time,
                before.commit_wal_time,
            ),
        ] {
            println!("{:<52} {:8.2} us", label, per(now, was));
        }
    }

    bench("INSERT one row", 5_000, || {
        db.query_params(
            "INSERT INTO p (name, price, bucket, rare) VALUES (?, ?, ?, ?)",
            &[
                Value::Text("x".into()),
                Value::Int64(1),
                Value::Int64(0),
                Value::Int64(0),
            ],
        )
        .expect("insert");
    });
}

/// The two terms a full scan is made of: what a row costs before any column
/// is looked at (reaching its visible version through the primary directory)
/// and what each additional column costs (decode plus materialize). A line is
/// fitted through four projection widths over the same 20 000 rows, so both
/// terms come from one set of measurements and neither depends on the other.
fn row_and_column_cost(dir: &std::path::Path) -> Db {
    const ROWS: i64 = 20_000;
    let db = Db::create_with(
        dir.join("wide.esql"),
        DbOptions {
            durability: Durability::Balanced,
            ..DbOptions::default()
        },
    )
    .expect("create wide");
    let columns: Vec<String> = (0..8).map(|i| format!("c{i}")).collect();
    db.query(&format!(
        "CREATE TABLE w (id int AUTO_INCREMENT PRIMARY KEY, {})",
        columns
            .iter()
            .map(|name| format!("{name} int NOT NULL"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .expect("ddl wide");
    let mut txn = db.begin();
    let placeholders = vec!["?"; columns.len()].join(", ");
    let insert = format!(
        "INSERT INTO w ({}) VALUES ({placeholders})",
        columns.join(", ")
    );
    for i in 0..ROWS {
        let params: Vec<Value> = (0..columns.len())
            .map(|c| Value::Int64(i + c as i64))
            .collect();
        txn.query_params(&insert, &params).expect("seed wide");
    }
    txn.commit().expect("commit wide");
    db.checkpoint().expect("checkpoint wide");

    println!("\n--- full scan of 20 000 rows, by projection width ---");
    // `count(*)` walks the same rows and decodes none of them, so it is the
    // cost of reaching a row's visible version and nothing else.
    let counted = bench("COUNT(*): no column decoded", 20, || {
        db.query("SELECT count(*) FROM w").expect("count");
    });
    let widths = [1usize, 2, 4, 8];
    let mut points = Vec::new();
    for width in widths {
        let statement = format!("SELECT {} FROM w", columns[..width].join(", "));
        let per_scan = bench(&format!("SELECT {width} of 8 columns"), 20, || {
            let QueryOutput::Rows { rows, .. } = db.query(&statement).expect("scan") else {
                panic!("expected rows")
            };
            assert_eq!(rows.len() as i64, ROWS);
        });
        let per_row_ns = per_scan * 1000.0 / ROWS as f64;
        points.push((width as f64, per_row_ns));
    }
    // Least squares through the four points: slope is the per-column term,
    // intercept the per-row term.
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let slope = points
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum::<f64>()
        / points
            .iter()
            .map(|(x, _)| (x - mean_x).powi(2))
            .sum::<f64>();
    let intercept = mean_y - slope * mean_x;
    println!(
        "{:<52} {:8.1} ns",
        "  per row, reaching the visible version",
        counted * 1000.0 / ROWS as f64
    );
    println!("{:<52} {intercept:8.1} ns", "  per row, before any column");
    println!("{:<52} {slope:8.1} ns", "  per column");
    db
}
