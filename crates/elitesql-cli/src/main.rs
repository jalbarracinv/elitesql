//! `elitesql` — the EliteSQL command line.
//!
//! Subcommands: query, repl, tables, check, compact, repair, export, import,
//! serve (Unix-socket sidecar for multi-process deployments), version.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use elitesql_core::{jsonio, Db, DbOptions, Durability, QueryOutput, Value};

mod serve;

const USAGE: &str = "\
elitesql — a tiny operational database for AI-native apps

USAGE:
  elitesql query <db> <sql>            execute one SQL statement
  elitesql repl <db>                   interactive SQL shell
  elitesql tables <db>                 list tables with their schemas
  elitesql check <db>                  offline integrity check
  elitesql compact <db>                compact segments and vector indexes
  elitesql repair <src> <dst>          salvage records into a fresh database
  elitesql export <db> <table>         records as JSON lines on stdout
  elitesql import <db> <table>         JSON lines from stdin into a table
  elitesql serve <db> <socket-path>    sidecar server over a Unix socket
  elitesql version

OPTIONS:
  --durability safe|balanced|fast    (query/repl/import/serve; default safe)
  --read-only                        open without touching disk; writes fail
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    // Extract global options.
    let mut durability = Durability::Safe;
    if let Some(i) = args.iter().position(|a| a == "--durability") {
        if i + 1 >= args.len() {
            return Err("--durability requires a value (safe|balanced|fast)".into());
        }
        durability = match args[i + 1].as_str() {
            "safe" => Durability::Safe,
            "balanced" => Durability::Balanced,
            "fast" => Durability::Fast,
            other => return Err(format!("unknown durability '{other}'")),
        };
        args.drain(i..=i + 1);
    }
    let mut read_only = false;
    if let Some(i) = args.iter().position(|a| a == "--read-only") {
        read_only = true;
        args.remove(i);
    }
    let opts = DbOptions { durability, read_only, ..DbOptions::default() };

    let cmd = args.first().cloned().unwrap_or_default();
    match cmd.as_str() {
        "query" => {
            let [db_path, sql] = take::<2>(&args)?;
            let db = open(&db_path, opts)?;
            print_output(db.query(&sql).map_err(|e| e.to_string())?);
            Ok(())
        }
        "repl" => {
            let [db_path] = take::<1>(&args)?;
            repl(&db_path, opts)
        }
        "tables" => {
            let [db_path] = take::<1>(&args)?;
            let db = open(&db_path, opts)?;
            for name in db.tables() {
                let schema = db.table_schema(&name).expect("listed");
                println!("{}", serde_json::to_string_pretty(&schema).unwrap());
            }
            Ok(())
        }
        "check" => {
            let [db_path] = take::<1>(&args)?;
            let report = elitesql_core::check(&db_path).map_err(|e| e.to_string())?;
            for w in &report.warnings {
                println!("warning: {w}");
            }
            for e in &report.errors {
                println!("ERROR: {e}");
            }
            if report.is_ok() {
                println!("ok: database validates");
                Ok(())
            } else {
                Err(format!("{} integrity error(s) found", report.errors.len()))
            }
        }
        "compact" => {
            let [db_path] = take::<1>(&args)?;
            let db = open(&db_path, opts)?;
            db.compact().map_err(|e| e.to_string())?;
            println!("ok: compacted");
            Ok(())
        }
        "repair" => {
            let [src, dst] = take::<2>(&args)?;
            let report = elitesql_core::salvage(&src, &dst).map_err(|e| e.to_string())?;
            println!("tables:             {}", report.tables.join(", "));
            println!("recovered records:  {}", report.recovered_records);
            println!("deleted (correct):  {}", report.deleted_records);
            println!("skipped:            {}", report.skipped);
            println!("segments scanned:   {}", report.segments_scanned);
            println!("wal files scanned:  {}", report.wal_files_scanned);
            for note in &report.notes {
                println!("note: {note}");
            }
            println!("salvaged into {dst}");
            Ok(())
        }
        "export" => {
            let [db_path, table] = take::<2>(&args)?;
            let db = open(&db_path, opts)?;
            let rows = db.scan(&table).map_err(|e| e.to_string())?;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for (_, record) in &rows {
                writeln!(out, "{}", jsonio::record_to_json(record)).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        "import" => {
            let [db_path, table] = take::<2>(&args)?;
            let db = open(&db_path, opts)?;
            import(&db, &table)
        }
        "serve" => {
            let [db_path, socket] = take::<2>(&args)?;
            let db = open(&db_path, opts)?;
            serve::serve(db, &socket)
        }
        "version" => {
            println!("elitesql {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "" | "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

/// Positional arguments after the subcommand, exactly N of them.
fn take<const N: usize>(args: &[String]) -> Result<[String; N], String> {
    let rest = &args[1..];
    if rest.len() != N {
        return Err(format!(
            "'{}' expects {N} argument(s), got {}\n\n{USAGE}",
            args[0],
            rest.len()
        ));
    }
    Ok(std::array::from_fn(|i| rest[i].clone()))
}

fn open(path: &str, opts: DbOptions) -> Result<Db, String> {
    if opts.read_only {
        Db::open_with(path, opts).map_err(|e| e.to_string())
    } else {
        Db::open_or_create_with(path, opts).map_err(|e| e.to_string())
    }
}

fn repl(path: &str, opts: DbOptions) -> Result<(), String> {
    let db = open(path, opts)?;
    eprintln!("elitesql {} — {} — .exit to quit", env!("CARGO_PKG_VERSION"), path);
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        eprint!("elitesql> ");
        std::io::stderr().flush().ok();
        line.clear();
        if stdin.lock().read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Ok(()); // EOF
        }
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        if sql == ".exit" || sql == ".quit" {
            return Ok(());
        }
        match db.query(sql) {
            Ok(out) => print_output(out),
            Err(e) => eprintln!("error: {e}"),
        }
    }
}

fn import(db: &Db, table: &str) -> Result<(), String> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| format!("table '{table}' does not exist; create it first (CREATE TABLE)"))?;
    let stdin = std::io::stdin();
    let mut txn = db.begin();
    let mut imported = 0u64;
    let mut in_batch = 0usize;
    for (line_no, line) in stdin.lock().lines().enumerate() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let j: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("line {}: {e}", line_no + 1))?;
        let obj = j
            .as_object()
            .ok_or_else(|| format!("line {}: expected a JSON object", line_no + 1))?;
        let mut record = elitesql_core::Record::new();
        for (k, v) in obj {
            let value = if k == "id" {
                match v.as_str() {
                    Some(s) => Value::Text(s.to_owned()),
                    None => return Err(format!("line {}: id must be a string", line_no + 1)),
                }
            } else {
                let col = schema
                    .column(k)
                    .ok_or_else(|| format!("line {}: unknown column '{k}'", line_no + 1))?;
                jsonio::json_to_value_for_type(v, col.ty)
                    .map_err(|e| format!("line {}: {e}", line_no + 1))?
            };
            record.insert(k.clone(), value);
        }
        txn.insert(table, record)
            .map_err(|e| format!("line {}: {e}", line_no + 1))?;
        imported += 1;
        in_batch += 1;
        if in_batch >= 1000 {
            txn.commit().map_err(|e| e.to_string())?;
            txn = db.begin();
            in_batch = 0;
        }
    }
    txn.commit().map_err(|e| e.to_string())?;
    eprintln!("imported {imported} record(s) into {table}");
    Ok(())
}

fn print_output(out: QueryOutput) {
    match out {
        QueryOutput::Rows { columns, rows } => {
            let rendered: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(short_value).collect())
                .collect();
            let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
            for row in &rendered {
                for (i, cell) in row.iter().enumerate() {
                    widths[i] = widths[i].max(cell.len());
                }
            }
            let header: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
                .collect();
            println!("{}", header.join("  "));
            println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
            for row in &rendered {
                let line: Vec<String> = row
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
                    .collect();
                println!("{}", line.join("  "));
            }
            println!("({} row(s))", rows.len());
        }
        QueryOutput::Inserted { ids } => {
            for id in &ids {
                println!("{id}");
            }
            println!("({} inserted)", ids.len());
        }
        QueryOutput::Affected(n) => println!("({n} affected)"),
        QueryOutput::None => println!("ok"),
    }
}

fn short_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("x'{}...' ({} bytes)", hex_prefix(b, 8), b.len()),
        Value::Timestamp(us) => jsonio::format_timestamp(*us),
        Value::Date(d) => jsonio::format_date(*d),
        Value::Time(t) => jsonio::format_time(*t),
        Value::Json(j) => j.to_string(),
        Value::Vector(v) => format!("[vector dim={}]", v.len()),
    }
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    bytes.iter().take(n).map(|b| format!("{b:02x}")).collect()
}
