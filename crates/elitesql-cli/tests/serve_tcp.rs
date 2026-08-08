//! `elitesql serve --tcp` over a real socket.
//!
//! The unit tests in `serve.rs` cover the auth decision; this one covers the
//! wiring around it — that the process binds, that a client on the other end of
//! a TCP connection gets through the handshake, and that the connection cap
//! actually refuses rather than queues.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_elitesql")
}

/// Starts the sidecar on a free port and waits until it accepts connections.
fn start(dir: &std::path::Path, token: &str, extra: &[&str]) -> Server {
    // Ask the OS for a free port, then release it: a fixed port would make the
    // suite fail when something else on the machine already holds it.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let db = dir.join("serve.esql");
    let mut command = Command::new(binary());
    command
        .arg("serve")
        .arg(&db)
        .arg("--tcp")
        .arg(format!("127.0.0.1:{port}"))
        .args(extra)
        .env("ELITESQL_TOKEN", token)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("spawn elitesql serve");
    let server = Server { child, port };

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return server;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("sidecar did not start listening on {port}");
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(port: u16) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn call(&mut self, request: serde_json::Value) -> serde_json::Value {
        writeln!(self.writer, "{request}").unwrap();
        self.writer.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response {line:?}: {e}"))
    }
}

#[test]
fn a_tcp_client_authenticates_then_queries() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "s3cr3t", &[]);
    let mut client = Client::connect(server.port);

    // Refused before the handshake.
    assert_eq!(client.call(serde_json::json!({"op": "ping"}))["ok"], false);

    let authed = client.call(serde_json::json!({"op": "auth", "token": "s3cr3t"}));
    assert_eq!(authed["ok"], true, "{authed}");

    let created = client.call(serde_json::json!({
        "op": "query",
        "sql": "CREATE TABLE t (name text, n int64)"
    }));
    assert_eq!(created["ok"], true, "{created}");

    let inserted = client.call(serde_json::json!({
        "op": "query",
        "sql": "INSERT INTO t (name, n) VALUES (%s, %s)",
        "params": ["ana", 42]
    }));
    assert_eq!(inserted["ok"], true, "{inserted}");

    let selected = client.call(serde_json::json!({
        "op": "query",
        "sql": "SELECT name, n FROM t WHERE n = 42"
    }));
    assert_eq!(selected["result"]["rows"][0][0], "ana");
    assert_eq!(selected["result"]["rows"][0][1], 42);

    // An engine error crosses the wire with its own code, not the auth code.
    let missing = client.call(serde_json::json!({
        "op": "query",
        "sql": "SELECT * FROM no_such_table"
    }));
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["code"], 4);
}

/// Authentication is per connection: a second client must do its own handshake.
#[test]
fn authentication_does_not_carry_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "s3cr3t", &[]);

    let mut first = Client::connect(server.port);
    assert_eq!(
        first.call(serde_json::json!({"op": "auth", "token": "s3cr3t"}))["ok"],
        true
    );

    let mut second = Client::connect(server.port);
    let response = second.call(serde_json::json!({"op": "ping"}));
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], 20);
}

#[test]
fn a_wrong_token_leaves_the_connection_closed_for_business() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "s3cr3t", &[]);
    let mut client = Client::connect(server.port);

    let denied = client.call(serde_json::json!({"op": "auth", "token": "guess"}));
    assert_eq!(denied["ok"], false);
    assert_eq!(denied["error"], "invalid token");

    // Still refused afterwards, and a retry with the right token is allowed:
    // a failed attempt must not lock the connection out permanently either.
    assert_eq!(client.call(serde_json::json!({"op": "ping"}))["ok"], false);
    assert_eq!(
        client.call(serde_json::json!({"op": "auth", "token": "s3cr3t"}))["ok"],
        true
    );
    assert_eq!(
        client.call(serde_json::json!({"op": "ping"}))["result"],
        "pong"
    );
}

/// The cap exists so a reachable port cannot exhaust the process's threads.
#[test]
fn the_connection_cap_refuses_instead_of_queueing() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "s3cr3t", &["--max-connections", "2"]);

    // Hold the cap open with two live connections.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut c = Client::connect(server.port);
        assert_eq!(
            c.call(serde_json::json!({"op": "auth", "token": "s3cr3t"}))["ok"],
            true
        );
        held.push(c);
    }

    // The third is answered with a refusal rather than left hanging.
    let mut extra = Client::connect(server.port);
    let refusal = extra.call(serde_json::json!({"op": "ping"}));
    assert_eq!(refusal["ok"], false, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("too many connections"),
        "{refusal}"
    );

    // Freeing a slot lets a new client in again.
    drop(held.pop());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut c = Client::connect(server.port);
        let response = c.call(serde_json::json!({"op": "auth", "token": "s3cr3t"}));
        if response["ok"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "slot never freed: {response}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// --- transport parity ---------------------------------------------------------

/// Owns the Unix-socket sidecar process, so a failed startup wait does not
/// leave it running behind the test.
struct UnixServer {
    child: Child,
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the sidecar on a Unix socket. The path lives in the short-lived
/// temp dir because AF_UNIX paths are capped around 104 bytes.
fn start_unix(socket: &std::path::Path, db: &std::path::Path) -> UnixServer {
    let child = Command::new(binary())
        .arg("serve")
        .arg(db)
        .arg(socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn elitesql serve");
    let server = UnixServer { child };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return server;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("sidecar did not start listening on {socket:?}");
}

/// Every generated id differs between two runs, so compare shapes rather than
/// values: a ULID becomes "<id>" wherever one appears.
fn normalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) if s.len() == 26 && s.chars().all(|c| c.is_alphanumeric()) => {
            serde_json::json!("<id>")
        }
        serde_json::Value::Array(items) => items.iter().map(normalize).collect(),
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(k, v)| (k.clone(), normalize(v)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        other => other.clone(),
    }
}

/// The operations a client would actually perform, covering DDL, writes,
/// parameters, reads, aggregates, planning and both kinds of failure.
fn parity_script() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"op": "ping"}),
        serde_json::json!({"op": "query", "sql": "CREATE TABLE users (name text NOT NULL, email text, age int64)"}),
        serde_json::json!({"op": "query", "sql": "CREATE UNIQUE INDEX ON users (email)"}),
        serde_json::json!({"op": "query", "sql": "INSERT INTO users (name, email, age) VALUES (%s, %s, %s)", "params": ["ana", "ana@x.com", 30]}),
        serde_json::json!({"op": "query", "sql": "INSERT INTO users (name, email, age) VALUES (%(n)s, %(e)s, %(a)s)", "params": {"n": "bob", "e": "bob@x.com", "a": 25}}),
        serde_json::json!({"op": "query", "sql": "SELECT name, age FROM users ORDER BY age DESC"}),
        serde_json::json!({"op": "query", "sql": "SELECT name FROM users WHERE email = %s", "params": ["ana@x.com"]}),
        serde_json::json!({"op": "query", "sql": "SELECT count(*), sum(age) FROM users"}),
        // Three-valued logic must not depend on the transport either.
        serde_json::json!({"op": "query", "sql": "SELECT name FROM users WHERE NOT age = 30"}),
        serde_json::json!({"op": "query", "sql": "EXPLAIN SELECT name FROM users WHERE email = %s", "params": ["ana@x.com"]}),
        serde_json::json!({"op": "query", "sql": "UPDATE users SET age = 31 WHERE name = 'ana'"}),
        serde_json::json!({"op": "query", "sql": "DELETE FROM users WHERE name = 'bob'"}),
        // Both failure kinds: an engine error and a rejected statement.
        serde_json::json!({"op": "query", "sql": "SELECT * FROM missing"}),
        serde_json::json!({"op": "query", "sql": "SELECT name FROM users LIMIT 10, 20"}),
        serde_json::json!({"op": "query", "sql": "INSERT INTO users (name, email) VALUES ('eva', 'ana@x.com')"}),
        serde_json::json!({"op": "checkpoint"}),
    ]
}

/// The promise the documentation makes: the transport carries the protocol and
/// nothing else. Same requests, same responses — only latency and the auth
/// handshake differ.
#[test]
fn tcp_and_unix_answer_identically() {
    let dir = tempfile::tempdir().unwrap();

    let tcp_server = start(dir.path(), "s3cr3t", &[]);
    let mut tcp = Client::connect(tcp_server.port);
    assert_eq!(
        tcp.call(serde_json::json!({"op": "auth", "token": "s3cr3t"}))["ok"],
        true
    );
    let over_tcp: Vec<serde_json::Value> = parity_script()
        .iter()
        .map(|r| normalize(&tcp.call(r.clone())))
        .collect();

    // A separate directory: one process owns a database, so the two servers
    // cannot share one. Identical schema and statements, identical answers.
    let unix_dir = tempfile::tempdir().unwrap();
    let socket = unix_dir.path().join("s");
    let unix_server = start_unix(&socket, &unix_dir.path().join("serve.esql"));
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let over_unix: Vec<serde_json::Value> = parity_script()
        .iter()
        .map(|request| {
            writeln!(writer, "{request}").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            normalize(&serde_json::from_str(&line).unwrap())
        })
        .collect();
    drop(unix_server);

    for ((request, tcp), unix) in parity_script().iter().zip(&over_tcp).zip(&over_unix) {
        assert_eq!(
            tcp, unix,
            "transports disagree on {request}:\n  tcp:  {tcp}\n  unix: {unix}"
        );
    }

    // Two identical lists of failures would also compare equal, so check the
    // script actually exercised the engine before believing the comparison.
    let script = parity_script();
    assert_eq!(over_tcp.len(), script.len());
    let succeeded = over_tcp.iter().filter(|r| r["ok"] == true).count();
    let failed = over_tcp.iter().filter(|r| r["ok"] == false).count();
    assert!(
        succeeded >= 12,
        "expected real work, got {succeeded} successes"
    );
    assert!(failed >= 3, "expected the error paths to run, got {failed}");
    // And that reads returned data rather than empty result sets.
    let ordered = &over_tcp[5];
    assert_eq!(ordered["result"]["rows"][0][0], "ana");
    assert_eq!(ordered["result"]["rows"][1][0], "bob");
    let plan = &over_tcp[9]["result"]["rows"][0][0];
    assert_eq!(plan, "INDEX LOOKUP users.email = 'ana@x.com'", "{plan}");
}
