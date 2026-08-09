use std::io::Write;
use std::process::{Command, Stdio};

use elitesql_core::Db;

#[test]
fn sqlite_style_repl_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repl.esql");
    let mut child = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .arg("--create")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"create table tbl1(one text, two int);\n\
              insert into tbl1 values('hello!',10),('goodbye',20);\n\
              select * from tbl1;\n\
              CREATE TABLE tbl2 (\n\
                f1 text,\n\
                f2 text,\n\
              f3 float64\n\
              );\n\
              .help\n\
              .exit\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    // The banner leads with the V<YYYYMMDD> build tag so a stale binary is
    // obvious at a glance, then the crate version and the exact build time.
    let banner = stdout.lines().next().unwrap_or_default();
    assert!(banner.starts_with("EliteSQL V"), "{stdout}");
    let tag = &banner["EliteSQL V".len().."EliteSQL V".len() + 8];
    assert!(
        tag.len() == 8 && tag.chars().all(|c| c.is_ascii_digit()),
        "build tag should be YYYYMMDD, got {tag:?} in {stdout}"
    );
    assert!(banner.contains(", built "), "{stdout}");
    assert!(
        stdout.contains("Enter \".help\" for usage hints."),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "┌───────────┬─────┐\n\
         │    one    │ two │\n\
         ├───────────┼─────┤\n\
         │ 'hello!'  │ 10  │\n\
         │ 'goodbye' │ 20  │\n\
         └───────────┴─────┘"
        ),
        "{stdout}"
    );
    assert_eq!(stdout.matches("   ...> ").count(), 4, "{stdout}");
    assert!(stdout.contains("EliteSQL interactive shell"), "{stdout}");
    assert!(stdout.contains("SUPPORTED STATEMENTS"), "{stdout}");
    assert!(
        stdout.contains("Run elitesql --help outside the shell"),
        "{stdout}"
    );

    let db = Db::open(path).unwrap();
    assert!(
        db.table_schema("tbl2").is_some(),
        "multiline CREATE TABLE executed"
    );
}

#[test]
fn command_help_identifies_the_elitesql_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("EliteSQL CLI — command-line shell and tools for EliteSQL"),
        "{stdout}"
    );
    assert!(stdout.contains("elitesql <db>"), "{stdout}");
}

#[test]
fn repl_waits_for_semicolons_outside_strings_and_comments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terminators.esql");
    let mut child = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .arg("--create")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"CREATE TABLE items(name text, n int);\n\
              INSERT INTO items\n\
              VALUES ('semi;colon', 1), -- this ; is a comment\n\
                     ('block', 2)\n\
              /* this ; is also a comment */;\n\
              SELECT\n\
                name,\n\
                n\n\
              FROM items\n\
              ORDER BY n\n\
              ;\n\
              CREATE TABLE extra(a int); INSERT INTO extra VALUES (1);\n\
              .exit\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(
            "┌──────────────┬───┐\n\
         │     name     │ n │\n\
         ├──────────────┼───┤\n\
         │ 'semi;colon' │ 1 │\n\
         │   'block'    │ 2 │\n\
         └──────────────┴───┘"
        ),
        "{stdout}"
    );

    let db = Db::open(path).unwrap();
    assert_eq!(db.scan("items").unwrap().len(), 2);
    assert_eq!(
        db.scan("extra").unwrap().len(),
        1,
        "two statements on one line execute"
    );
}

#[test]
fn repl_does_not_execute_an_unterminated_statement_at_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eof.esql");
    let mut child = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .arg("--create")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"CREATE TABLE should_not_exist(a int)\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let db = Db::open(path).unwrap();
    assert!(db.table_schema("should_not_exist").is_none());
}

/// A single argument that is not a subcommand is read as a database path, so a
/// mistyped subcommand used to leave a directory named after the typo. Opening
/// no longer creates anything unless asked.
#[test]
fn opening_does_not_create_a_database_by_accident() {
    let dir = tempfile::tempdir().unwrap();
    let typo = dir.path().join("versio");

    let output = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .arg(&typo)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(stderr.contains("--create"), "{stderr}");
    assert!(!typo.exists(), "a refused open must leave nothing behind");

    // Subcommands that open a database refuse the same way.
    let output = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .args(["query"])
        .arg(&typo)
        .arg("SELECT 1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!typo.exists());

    // With --create it is created, and afterwards opening needs no flag.
    let output = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .arg("--create")
        .args(["query"])
        .arg(&typo)
        .arg("CREATE TABLE t (a int)")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(typo.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_elitesql"))
        .args(["query"])
        .arg(&typo)
        .arg("SELECT a FROM t")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
