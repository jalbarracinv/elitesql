//! `elitesql` — the EliteSQL command line.
//!
//! Subcommands: query, repl, tables, check, compact, backup, restore, repair,
//! export, import, serve (sidecar over a Unix socket for multi-process
//! deployments, or over TCP for another host), version.
//!
//! Opening a database never creates it; `--create` does. A single argument that
//! is not a subcommand is read as a path, so without that a mistyped subcommand
//! would leave a database directory named after the typo.

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
  elitesql serve <db> --tcp <addr>     sidecar server over TCP (needs a token)
  elitesql version

OPTIONS:
  --durability safe|balanced|fast    (query/repl/import/serve; default safe)
  --read-only                        open without touching disk; writes fail
  --create                           create the database if it does not exist
  --full-fsync                       flush the drive cache at every barrier
                                     (macOS; needed for safe to survive power
                                     loss there; much slower)
  --batch <n>                        import: commit every n rows instead of
                                     one transaction for the whole input

EXIT STATUS:
  0 success   1 failure   3 completed with warnings (check/restore/export
  --read-only over a damaged database)   4 repair finished but skipped entries

  Opening never creates a database on its own: a mistyped subcommand is read
  as a path, and would otherwise leave a directory named after the typo.

SERVE OPTIONS:
  --tcp <host:port>                  listen on TCP instead of a Unix socket
  --token-file <path>                shared secret; required with --tcp
  --max-connections <n>              concurrent connection cap (default 128)

  A Unix socket is authenticated by filesystem permissions. TCP is not, so it
  requires a token, read from --token-file or the ELITESQL_TOKEN environment
  variable (never a flag, which `ps` would expose). Traffic is unencrypted:
  bind loopback and use an SSH tunnel or a VPN to reach another host.
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
  EXPLAIN SELECT ...;           Show the plan without running the query
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

/// Exit status when a command completed but the result needs attention: the
/// database validated with warnings, a restore carried warnings, a read-only
/// export exposed a partial state. Scripts must treat it as "look before you
/// trust", distinct from 0 (clean) and 1 (failed).
const EXIT_WARNINGS: u8 = 3;
/// Exit status for a salvage that finished but could not recover every entry.
const EXIT_PARTIAL: u8 = 4;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(mut args: Vec<String>) -> Result<ExitCode, String> {
    let code = run_command(&mut args)?;
    Ok(code)
}

fn run_command(args: &mut Vec<String>) -> Result<ExitCode, String> {
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
    let mut create = false;
    if let Some(i) = args.iter().position(|a| a == "--create") {
        create = true;
        args.remove(i);
    }
    let mut full_fsync = false;
    if let Some(i) = args.iter().position(|a| a == "--full-fsync") {
        full_fsync = true;
        args.remove(i);
    }
    let mut import_batch: Option<usize> = None;
    if let Some(i) = args.iter().position(|a| a == "--batch") {
        let value = args
            .get(i + 1)
            .ok_or("--batch requires a row count")?
            .parse::<usize>()
            .ok()
            .filter(|rows| *rows > 0)
            .ok_or("--batch requires a positive row count")?;
        import_batch = Some(value);
        args.drain(i..=i + 1);
    }
    let opts = DbOptions {
        durability,
        read_only,
        full_fsync,
        ..DbOptions::default()
    };
    let args: &Vec<String> = args;

    let cmd = args.first().cloned().unwrap_or_default();
    match cmd.as_str() {
        "query" => {
            let [db_path, sql] = take::<2>(args)?;
            let db = open(&db_path, opts, create)?;
            let out = db.query(&sql).map_err(|e| e.to_string())?;
            match out {
                QueryOutput::Rows { rows, .. } if is_explain(sql.trim()) => print_plan(&rows),
                other => print_output(other),
            }
            Ok(ExitCode::SUCCESS)
        }
        "repl" => {
            let [db_path] = take::<1>(args)?;
            repl(&db_path, opts, create)?;
            Ok(ExitCode::SUCCESS)
        }
        "tables" => {
            let [db_path] = take::<1>(args)?;
            let db = open(&db_path, opts, create)?;
            for name in db.tables() {
                let schema = db.table_schema(&name).expect("listed");
                println!("{}", serde_json::to_string_pretty(&schema).unwrap());
            }
            Ok(ExitCode::SUCCESS)
        }
        "check" => {
            let [db_path] = take::<1>(args)?;
            let report = elitesql_core::check(&db_path).map_err(|e| e.to_string())?;
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            for e in &report.errors {
                eprintln!("ERROR: {e}");
            }
            if !report.is_ok() {
                return Err(format!("{} integrity error(s) found", report.errors.len()));
            }
            if report.warnings.is_empty() {
                println!("ok: database validates");
                Ok(ExitCode::SUCCESS)
            } else {
                println!(
                    "ok: database validates with {} warning(s); derived indexes may be rebuilt on open",
                    report.warnings.len()
                );
                Ok(ExitCode::from(EXIT_WARNINGS))
            }
        }
        "compact" => {
            let [db_path] = take::<1>(args)?;
            let db = open(&db_path, opts, create)?;
            db.compact().map_err(|e| e.to_string())?;
            println!("ok: compacted");
            Ok(ExitCode::SUCCESS)
        }
        "backup" => {
            let [db_path, dst] = take::<2>(args)?;
            let db = open(&db_path, opts, create)?;
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
            Ok(ExitCode::SUCCESS)
        }
        "restore" => {
            let [src, dst] = take::<2>(args)?;
            let report = elitesql_core::restore(&src, &dst).map_err(|e| e.to_string())?;
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            println!(
                "ok: restored {} table(s), {} record(s) into {dst}",
                report.tables, report.records
            );
            Ok(if report.warnings.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EXIT_WARNINGS)
            })
        }
        "repair" => {
            let [src, dst] = take::<2>(args)?;
            let report = elitesql_core::salvage(&src, &dst).map_err(|e| e.to_string())?;
            println!("tables:             {}", report.tables.join(", "));
            println!("recovered records:  {}", report.recovered_records);
            println!("deleted (correct):  {}", report.deleted_records);
            println!("skipped:            {}", report.skipped);
            println!("segments scanned:   {}", report.segments_scanned);
            println!("wal files scanned:  {}", report.wal_files_scanned);
            for note in &report.notes {
                eprintln!("note: {note}");
            }
            if report.skipped > 0 || !report.notes.is_empty() {
                println!(
                    "salvaged into {dst} (PARTIAL: {} entr{} skipped, {} note(s))",
                    report.skipped,
                    if report.skipped == 1 { "y" } else { "ies" },
                    report.notes.len()
                );
                Ok(ExitCode::from(EXIT_PARTIAL))
            } else {
                println!("salvaged into {dst}");
                Ok(ExitCode::SUCCESS)
            }
        }
        "export" => {
            let [db_path, table] = take::<2>(args)?;
            let db = open(&db_path, opts, create)?;
            // A read-only open exposes the valid prefix of damaged files. Say
            // so, on stderr and in the exit status, so the export is never
            // mistaken for a complete copy.
            let partial = db.recovery_warnings();
            for warning in &partial {
                eprintln!("warning: {warning}");
            }
            let snapshot = db.snapshot();
            let mut cursor = None;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            loop {
                let rows = db
                    .scan_batch_at_bytes(
                        &snapshot,
                        &table,
                        cursor.as_deref(),
                        512,
                        (db.memory_options().query_working_bytes / 2).max(1),
                    )
                    .map_err(|e| e.to_string())?;
                if rows.is_empty() {
                    break;
                }
                for (_, record) in &rows {
                    writeln!(out, "{}", jsonio::record_to_json(record))
                        .map_err(|e| e.to_string())?;
                }
                cursor = rows.last().map(|(id, _)| id.clone());
            }
            Ok(if partial.is_empty() {
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "warning: export is PARTIAL: the database opened read-only over damaged files"
                );
                ExitCode::from(EXIT_WARNINGS)
            })
        }
        "import" => {
            let [db_path, table] = take::<2>(args)?;
            let db = open(&db_path, opts, create)?;
            import(&db, &table, import_batch)?;
            Ok(ExitCode::SUCCESS)
        }
        "serve" => {
            let mut args = args.clone();
            let tcp = take_option(&mut args, "--tcp")?;
            let token_file = take_option(&mut args, "--token-file")?;
            let max_connections = match take_option(&mut args, "--max-connections")? {
                Some(value) => value
                    .parse::<usize>()
                    .map_err(|_| format!("--max-connections expects a number, got '{value}'"))?,
                None => serve::ServeOptions::default().max_connections,
            };
            // A token on the command line is visible to every process on the
            // host through `ps`, so it is read from a file or the environment.
            let token = match token_file {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot read token file '{path}': {e}"))?
                        .trim()
                        .to_string(),
                ),
                None => std::env::var("ELITESQL_TOKEN").ok(),
            };
            // args is ["serve", <db>] plus, without --tcp, the socket path.
            let socket_path = if tcp.is_some() {
                let [_] = take::<1>(&args).map_err(|_| {
                    "with --tcp, 'serve' expects only the database path".to_string()
                })?;
                None
            } else {
                Some(take::<2>(&args)?[1].clone())
            };
            let db_path = args[1].clone();
            let db = open(&db_path, opts, create)?;
            serve::serve(
                db,
                serve::ServeOptions {
                    socket_path,
                    tcp,
                    token,
                    max_connections,
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        "version" => {
            println!("{}", version_banner());
            Ok(ExitCode::SUCCESS)
        }
        "" | "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        path if args.len() == 1 => {
            repl(path, opts, create)?;
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

/// Removes `--name value` from `args` and returns the value.
fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    if i + 1 >= args.len() {
        return Err(format!("{name} requires a value"));
    }
    let value = args[i + 1].clone();
    args.drain(i..=i + 1);
    Ok(Some(value))
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

/// Opens a database, creating it only when asked to.
///
/// A single argument that is not a known subcommand is treated as a database
/// path, so a mistyped subcommand (`elitesql versio`) would otherwise create a
/// directory named after the typo, silently and in the current working
/// directory. Creation is therefore opt-in through `--create`.
fn open(path: &str, opts: DbOptions, create: bool) -> Result<Db, String> {
    if opts.read_only {
        return Db::open_with(path, opts).map_err(|e| e.to_string());
    }
    if !create && !std::path::Path::new(path).exists() {
        return Err(format!(
            "'{path}' does not exist\n       to create it: elitesql --create {path}"
        ));
    }
    Db::open_or_create_with(path, opts).map_err(|e| e.to_string())
}

/// Startup identity line. The `V<YYYYMMDD>` build tag leads because it is what
/// answers "am I running the build I just made?" at a glance; the crate version
/// and the exact UTC build time follow for when the day is not enough.
fn version_banner() -> String {
    format!(
        "EliteSQL {} ({}, built {} UTC)",
        env!("ELITESQL_BUILD_DATE"),
        env!("CARGO_PKG_VERSION"),
        env!("ELITESQL_BUILD_TIMESTAMP")
    )
}

fn repl(path: &str, opts: DbOptions, create: bool) -> Result<(), String> {
    let db = open(path, opts, create)?;
    println!("{}", version_banner());
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
            if is_explain(sql.trim()) {
                print_plan(&rows);
                return;
            }
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

/// Load JSON lines into `table`. By default the whole input is one
/// transaction: a bad line leaves nothing behind and the command can simply
/// be rerun. `--batch N` trades that for bounded staging memory; on error the
/// message then states how many rows were already committed, so the caller
/// knows the file must not be replayed from the start without explicit ids.
fn import(db: &Db, table: &str, batch: Option<usize>) -> Result<(), String> {
    let schema = db
        .table_schema(table)
        .ok_or_else(|| format!("table '{table}' does not exist; create it first (CREATE TABLE)"))?;
    let stdin = std::io::stdin();
    let mut txn = db.begin();
    let mut imported = 0u64;
    let mut committed = 0u64;
    let mut in_batch = 0usize;
    let committed_hint = |committed: u64| {
        if committed > 0 {
            format!("; {committed} row(s) from earlier batches are already committed, do not replay them")
        } else {
            String::from("; nothing was committed")
        }
    };
    for (line_no, line) in stdin.lock().lines().enumerate() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let fail = |message: String| {
            format!(
                "line {}: {message}{}",
                line_no + 1,
                committed_hint(committed)
            )
        };
        let j: serde_json::Value = serde_json::from_str(&line).map_err(|e| fail(e.to_string()))?;
        let obj = j
            .as_object()
            .ok_or_else(|| fail("expected a JSON object".into()))?;
        let mut record = elitesql_core::Record::new();
        for (k, v) in obj {
            let value = if k == "id" && schema.column("id").is_none() {
                match v.as_str() {
                    Some(s) => Value::Text(s.to_owned()),
                    None => return Err(fail("id must be a string".into())),
                }
            } else {
                let col = schema
                    .column(k)
                    .ok_or_else(|| fail(format!("unknown column '{k}'")))?;
                jsonio::json_to_value_for_type(v, col.ty).map_err(|e| fail(e.to_string()))?
            };
            record.insert(k.clone(), value);
        }
        txn.insert(table, record).map_err(|e| fail(e.to_string()))?;
        imported += 1;
        in_batch += 1;
        if batch.is_some_and(|rows| in_batch >= rows) {
            txn.commit()
                .map_err(|e| format!("{e}{}", committed_hint(committed)))?;
            committed = imported;
            txn = db.begin();
            in_batch = 0;
        }
    }
    txn.commit()
        .map_err(|e| format!("{e}{}", committed_hint(committed)))?;
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
        QueryOutput::InsertedIdentity {
            ids,
            column,
            values,
        } => {
            for (id, value) in ids.iter().zip(&values) {
                println!("{id}\t{column}={value}");
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

/// An EXPLAIN plan is a tree encoded as indented text: the box renderer would
/// quote every line and centering would throw away the indentation that carries
/// the structure, so print the lines verbatim instead.
fn print_plan(rows: &[Vec<Value>]) {
    for row in rows {
        match row.first() {
            Some(Value::Text(line)) => println!("{line}"),
            other => println!("{}", other.map(short_value).unwrap_or_default()),
        }
    }
    println!();
}

fn is_explain(sql: &str) -> bool {
    sql.split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("explain"))
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
