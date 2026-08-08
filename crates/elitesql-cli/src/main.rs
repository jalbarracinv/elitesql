//! `elitesql` — the EliteSQL command line.
//!
//! Subcommands: query, repl, tables, check, compact, backup, restore, repair,
//! export, import, serve (Unix-socket sidecar for multi-process deployments),
//! version.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use elitesql_core::{jsonio, Db, DbOptions, Durability, QueryOutput, Value};

mod serve;

const USAGE: &str = "\
EliteSQL CLI — command-line shell and tools for EliteSQL

USAGE:
  elitesql <db>                        open the interactive SQL shell
  elitesql query <db> <sql>            execute one SQL statement
  elitesql repl <db>                   interactive SQL shell
  elitesql tables <db>                 list tables with their schemas
  elitesql check <db>                  offline integrity check
  elitesql compact <db>                compact segments and vector indexes
  elitesql backup <db> <dst>           snapshot-consistent copy, then verified
  elitesql restore <backup> <dst>      validate a backup and materialize it
  elitesql repair <src> <dst>          salvage records into a fresh database
  elitesql export <db> <table>         records as JSON lines on stdout
  elitesql import <db> <table>         JSON lines from stdin into a table
  elitesql serve <db> <socket-path>    sidecar server over a Unix socket
  elitesql version

OPTIONS:
  --durability safe|balanced|fast    (query/repl/import/serve; default safe)
  --read-only                        open without touching disk; writes fail
";

const REPL_HELP: &str = "\
EliteSQL interactive shell

SHELL COMMANDS
  .help                         Show this help
  .exit                         Exit the shell
  .quit                         Exit the shell

SQL INPUT
  End every SQL statement with ;
  Statements may span multiple lines. Semicolons inside quoted strings,
  -- line comments, and /* block comments */ do not end the statement.

SUPPORTED STATEMENTS
  CREATE TABLE table (column type, ...);
  CREATE [UNIQUE] INDEX [name] ON table (column);
  INSERT INTO table [(column, ...)] VALUES (...), (...);
  SELECT ... FROM ... [JOIN ...] [WHERE ...] [GROUP BY ...]
         [HAVING ...] [ORDER BY ...] [LIMIT ...] [OFFSET ...];
  UPDATE table SET column = value [, ...] [WHERE ...];
  DELETE FROM table [WHERE ...];
  DROP TABLE [IF EXISTS] table;
  DROP INDEX [name] ON table (column);
  ALTER TABLE table ADD [COLUMN] column type [NOT NULL] [DEFAULT value];
  ALTER TABLE table DROP [COLUMN] column;
  ALTER TABLE table RENAME [COLUMN] column TO new_name;
  ALTER TABLE table RENAME TO new_name;

DATA TYPES
  bool  int  float64  text  blob  timestamp  date  time  json  vector(N)
  integer, bigint, and int64 are aliases for int.
  A vector literal is a JSON array in a string: '[0.1, 0.2, 0.3]'.
  Vector and text search live in the API, not in SQL (see manual.md).

SCHEMA NOTES
  Every table has an implicit text id. SELECT * hides it in this shell;
  select id explicitly to display it. Use CREATE UNIQUE INDEX for uniqueness.

EXAMPLE
  CREATE TABLE notes(body text NOT NULL, score int);
  INSERT INTO notes VALUES ('hello', 10);
  SELECT * FROM notes;

Run elitesql --help outside the shell for database maintenance commands.
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
    let opts = DbOptions {
        durability,
        read_only,
        ..DbOptions::default()
    };

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
        "backup" => {
            let [db_path, dst] = take::<2>(&args)?;
            let db = open(&db_path, opts)?;
            let report = db.backup(&dst).map_err(|e| e.to_string())?;
            let check = elitesql_core::check(&dst).map_err(|e| e.to_string())?;
            if !check.is_ok() {
                return Err(format!(
                    "backup written but failed verification: {}",
                    check.errors.join("; ")
                ));
            }
            println!(
                "ok: backed up {} table(s), {} record(s) into {dst} (verified)",
                report.tables, report.records
            );
            Ok(())
        }
        "restore" => {
            let [src, dst] = take::<2>(&args)?;
            let report = elitesql_core::restore(&src, &dst).map_err(|e| e.to_string())?;
            for w in &report.warnings {
                println!("warning: {w}");
            }
            println!(
                "ok: restored {} table(s), {} record(s) into {dst}",
                report.tables, report.records
            );
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
            println!(
                "EliteSQL version {} {}",
                env!("CARGO_PKG_VERSION"),
                env!("ELITESQL_BUILD_TIMESTAMP")
            );
            Ok(())
        }
        "" | "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        path if args.len() == 1 => repl(path, opts),
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
    println!(
        "EliteSQL version {} {}",
        env!("CARGO_PKG_VERSION"),
        env!("ELITESQL_BUILD_TIMESTAMP")
    );
    println!("Enter \".help\" for usage hints.");
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    let mut sql = String::new();
    loop {
        if sql.is_empty() {
            print!("elitesql> ");
        } else {
            print!("   ...> ");
        }
        std::io::stdout().flush().ok();
        line.clear();
        if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Ok(()); // EOF
        }
        let trimmed = line.trim();
        if sql.is_empty() && trimmed.is_empty() {
            continue;
        }
        if sql.is_empty() && trimmed.starts_with('.') {
            match trimmed {
                ".exit" | ".quit" => return Ok(()),
                ".help" => print!("{REPL_HELP}"),
                command => eprintln!("error: unknown command '{command}'; enter .help for help"),
            }
            continue;
        }
        sql.push_str(&line);
        for statement in take_complete_statements(&mut sql) {
            execute_repl_statement(&db, &statement);
        }
    }
}

fn execute_repl_statement(db: &Db, sql: &str) {
    match db.query(sql.trim()) {
        Ok(QueryOutput::Rows {
            mut columns,
            mut rows,
        }) => {
            if is_star_select(sql) && columns.first().map(String::as_str) == Some("id") {
                columns.remove(0);
                for row in &mut rows {
                    row.remove(0);
                }
            }
            print_rows(&columns, &rows);
        }
        Ok(_) => {}
        Err(e) => eprintln!("error: {e}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlScanState {
    Normal,
    Quoted,
    LineComment,
    BlockComment,
}

fn scan_sql(sql: &str) -> (Vec<usize>, SqlScanState) {
    let bytes = sql.as_bytes();
    let mut ends = Vec::new();
    let mut state = SqlScanState::Normal;
    let mut i = 0;
    while i < bytes.len() {
        match state {
            SqlScanState::Quoted if bytes[i] == b'\'' => {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                state = SqlScanState::Normal;
            }
            SqlScanState::Quoted => {}
            SqlScanState::LineComment if bytes[i] == b'\n' => {
                state = SqlScanState::Normal;
            }
            SqlScanState::LineComment => {}
            SqlScanState::BlockComment if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') => {
                state = SqlScanState::Normal;
                i += 1;
            }
            SqlScanState::BlockComment => {}
            SqlScanState::Normal if bytes[i] == b'\'' => state = SqlScanState::Quoted,
            SqlScanState::Normal if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') => {
                state = SqlScanState::LineComment;
                i += 1;
            }
            SqlScanState::Normal if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') => {
                state = SqlScanState::BlockComment;
                i += 1;
            }
            SqlScanState::Normal if bytes[i] == b';' => ends.push(i + 1),
            SqlScanState::Normal => {}
        }
        i += 1;
    }
    (ends, state)
}

fn take_complete_statements(sql: &mut String) -> Vec<String> {
    let (ends, _) = scan_sql(sql);
    let mut statements = Vec::new();
    let mut start = 0;
    for end in ends {
        let statement = sql[start..end].trim();
        if has_sql_code(statement) {
            statements.push(statement.to_owned());
        }
        start = end;
    }
    if start > 0 {
        sql.drain(..start);
    }

    let (_, remainder_state) = scan_sql(sql);
    if !has_sql_code(sql)
        && !matches!(
            remainder_state,
            SqlScanState::Quoted | SqlScanState::BlockComment
        )
    {
        sql.clear();
    }
    statements
}

fn has_sql_code(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut state = SqlScanState::Normal;
    let mut i = 0;
    while i < bytes.len() {
        match state {
            SqlScanState::LineComment if bytes[i] == b'\n' => {
                state = SqlScanState::Normal;
            }
            SqlScanState::LineComment => {}
            SqlScanState::BlockComment if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') => {
                state = SqlScanState::Normal;
                i += 1;
            }
            SqlScanState::BlockComment => {}
            SqlScanState::Normal if bytes[i].is_ascii_whitespace() || bytes[i] == b';' => {}
            SqlScanState::Normal if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') => {
                state = SqlScanState::LineComment;
                i += 1;
            }
            SqlScanState::Normal if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') => {
                state = SqlScanState::BlockComment;
                i += 1;
            }
            SqlScanState::Normal | SqlScanState::Quoted => return true,
        }
        i += 1;
    }
    false
}

fn is_star_select(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("select"))
        && words.next() == Some("*")
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("from"))
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
        QueryOutput::Rows { columns, rows } => print_rows(&columns, &rows),
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

fn print_rows(columns: &[String], rows: &[Vec<Value>]) {
    println!("{}\n", render_table(columns, rows));
}

fn render_table(columns: &[String], rows: &[Vec<Value>]) -> String {
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(short_value).collect())
        .collect();
    let mut widths: Vec<usize> = columns.iter().map(|column| display_width(column)).collect();
    for row in &rendered {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
    }

    let border = |left: char, middle: char, right: char| {
        format!(
            "{left}{}{right}",
            widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join(&middle.to_string())
        )
    };
    let row = |cells: &[String]| {
        format!(
            "│{}│",
            cells
                .iter()
                .enumerate()
                .map(|(index, cell)| format!(" {} ", center(cell, widths[index])))
                .collect::<Vec<_>>()
                .join("│")
        )
    };

    let mut lines = Vec::with_capacity(rendered.len() + 4);
    lines.push(border('┌', '┬', '┐'));
    lines.push(row(columns));
    lines.push(border('├', '┼', '┤'));
    lines.extend(rendered.iter().map(|cells| row(cells)));
    lines.push(border('└', '┴', '┘'));
    lines.join("\n")
}

fn display_width(value: &str) -> usize {
    value.chars().count()
}

fn center(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(value));
    let left = padding / 2;
    let right = padding - left;
    format!("{}{value}{}", " ".repeat(left), " ".repeat(right))
}

fn short_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''").replace('\n', "\\n")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_repl_box_format() {
        let columns = vec!["one".into(), "two".into()];
        let rows = vec![
            vec![Value::Text("hello!".into()), Value::Int64(10)],
            vec![Value::Text("goodbye".into()), Value::Int64(20)],
        ];
        assert_eq!(
            render_table(&columns, &rows),
            "┌───────────┬─────┐\n\
             │    one    │ two │\n\
             ├───────────┼─────┤\n\
             │ 'hello!'  │ 10  │\n\
             │ 'goodbye' │ 20  │\n\
             └───────────┴─────┘"
        );
    }

    #[test]
    fn extracts_only_semicolon_terminated_statements() {
        let mut sql = "SELECT one,\n  two\nFROM tbl1".to_owned();
        assert!(take_complete_statements(&mut sql).is_empty());
        assert_eq!(sql, "SELECT one,\n  two\nFROM tbl1");

        sql.push_str(";\n");
        assert_eq!(
            take_complete_statements(&mut sql),
            ["SELECT one,\n  two\nFROM tbl1;"]
        );
        assert!(sql.is_empty());
    }

    #[test]
    fn ignores_semicolons_in_strings_and_comments() {
        let mut sql = "INSERT INTO t VALUES ('a;''b') -- ; line comment\n\
                       /* ; block comment */; SELECT * FROM t; trailing"
            .to_owned();
        let statements = take_complete_statements(&mut sql);
        assert_eq!(statements.len(), 2);
        assert_eq!(
            statements[0],
            "INSERT INTO t VALUES ('a;''b') -- ; line comment\n\
                                  /* ; block comment */;"
        );
        assert_eq!(statements[1], "SELECT * FROM t;");
        assert_eq!(sql, " trailing");
    }

    #[test]
    fn keeps_an_open_block_comment_in_the_pending_buffer() {
        let mut sql = "/* comment ;".to_owned();
        assert!(take_complete_statements(&mut sql).is_empty());
        assert_eq!(sql, "/* comment ;");

        sql.push_str(" */ SELECT * FROM t; -- finished comment\n");
        let statements = take_complete_statements(&mut sql);
        assert_eq!(statements, ["/* comment ; */ SELECT * FROM t;"]);
        assert!(sql.is_empty());
    }

    #[test]
    fn recognizes_plain_star_selects() {
        assert!(is_star_select(" SELECT * FROM tbl1;"));
        assert!(!is_star_select("SELECT id, * FROM tbl1;"));
    }
}
